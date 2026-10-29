//! External fixtures depend on Bee's documented CLI and ACP contract only.

use std::{collections::BTreeMap, future::Future, net::SocketAddr, path::PathBuf, time::Duration};

use agent_client_protocol::{
    Agent, Client, Error, Responder, V2ConnectionTo,
    schema::{ProtocolVersion, v2},
};
use agent_client_protocol_http::HttpClient;
use hive_daemon::config::{Config, Profile};
use serde_json::{Value, json};
use tokio::{sync::mpsc, time::timeout};

pub type Result<T = ()> = agent_client_protocol::Result<T>;

pub enum Event {
    Update(v2::UpdateSessionNotification),
    Permission(
        v2::RequestPermissionRequest,
        Responder<v2::RequestPermissionResponse>,
    ),
    Form(
        v2::CreateElicitationRequest,
        Responder<v2::CreateElicitationResponse>,
    ),
    Boundary,
}

#[derive(Clone)]
pub struct Gateway {
    pub address: SocketAddr,
    pub cwd: PathBuf,
    pub database: PathBuf,
}

pub async fn run<F, Fut>(scenario: &'static str, flow: F)
where
    F: FnOnce(Gateway) -> Fut + Send + 'static,
    Fut: Future<Output = Result> + Send + 'static,
{
    let command = std::env::var_os("HIVE_BEE_COMMAND")
        .unwrap_or_else(|| "bee".into())
        .into_string()
        .expect("HIVE_BEE_COMMAND must be a Unicode executable path");
    let version = timeout(
        Duration::from_secs(5),
        tokio::process::Command::new(&command)
            .arg("--version")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("Bee version timed out; run mise install --force bee")
    .expect("Bee unavailable; run mise install and activate mise or set HIVE_BEE_COMMAND");
    let identity = String::from_utf8_lossy(&version.stdout);
    assert!(
        version.status.success() && identity.trim().starts_with("bee "),
        "Expected Bee --version; reinstall the pinned test agent"
    );
    eprintln!("BEE scenario={scenario} version={}", identity.trim());

    let directory = tempfile::tempdir().unwrap();
    // Canonical paths also remove Windows extended-path ambiguity at the protocol boundary.
    let cwd = directory.path().canonicalize().unwrap();
    // Let Hive create its private state directory with its required permissions.
    let database = cwd.join("hive/sessions.sqlite3");
    let profile: Profile = serde_json::from_value(json!({
        "command": command, "args": ["--scenario", scenario], "versions": ["v2"]
    }))
    .unwrap();
    let broker = hive_daemon::Broker::persistent(
        Config {
            projects: Default::default(),
            default_agent: Some("bee".into()),
            agents: BTreeMap::from([("bee".into(), profile)]),
        },
        database.clone(),
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let origin = format!("http://{address}");
    let server = tokio::spawn(
        axum::serve(
            listener,
            hive_daemon::router_with_origin(broker.clone(), Some(&origin)),
        )
        .into_future(),
    );
    // Catch test panics in a separate task so worker cleanup also runs on assertion failure.
    let mut task = tokio::spawn(flow(Gateway {
        address,
        cwd,
        database,
    }));
    let result = timeout(Duration::from_secs(60), &mut task).await;
    if result.is_err() {
        task.abort();
        let _ = task.await;
    }
    let cleanup = timeout(Duration::from_secs(8), broker.shutdown()).await;
    server.abort();
    let _ = server.await;
    assert!(
        cleanup.is_ok(),
        "Bee scenario={scenario}: broker shutdown timed out"
    );
    match result {
        Ok(Ok(Ok(()))) => {}
        Ok(Ok(Err(error))) => panic!("Bee scenario={scenario}: ACP failure code={:?}", error.code),
        Ok(Err(error)) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
        Ok(Err(_)) => panic!("Bee scenario={scenario}: test flow was cancelled"),
        Err(_) => panic!("Bee scenario={scenario}: protocol deadline exceeded"),
    }
}

pub struct Peer {
    pub cx: V2ConnectionTo<Agent>,
    pub rx: mpsc::Receiver<Event>,
    boundary: mpsc::Sender<Event>,
    recent: std::collections::VecDeque<String>,
}

impl Gateway {
    pub fn identity_and_receipts(&self, id: &v2::SessionId) -> Value {
        let db = rusqlite::Connection::open_with_flags(
            &self.database,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let saved: String = db
            .query_row(
                "SELECT record FROM sessions WHERE id=?1",
                [id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        let saved: Value = serde_json::from_str(&saved).unwrap();
        let mut statement = db
            .prepare("SELECT key,result,terminal FROM prompts WHERE session_id=?1 ORDER BY key")
            .unwrap();
        let receipts: Vec<Value> = statement
            .query_map([id.to_string()], |row| {
                Ok(json!([
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?
                ]))
            })
            .unwrap()
            .map(|row| row.unwrap())
            .collect();
        // Ownership generations and replay sequence reservations may change on attachment;
        // native identity, workspace, and completed prompt evidence must not.
        json!({"id": saved["id"], "nativeId": saved["downstream_id"], "cwd": saved["cwd"], "receipts": receipts})
    }

    pub async fn client<F, Fut>(&self, scheme: &str, forms: bool, flow: F) -> Result
    where
        F: FnOnce(Peer) -> Fut + Send + 'static,
        Fut: Future<Output = Result> + Send,
    {
        eprintln!("BEE transport={scheme} forms={forms}");
        let (tx, rx) = mpsc::channel(512);
        let updates = tx.clone();
        let permissions = tx.clone();
        let forms_tx = tx.clone();
        Client
            .v2()
            .on_receive_notification(
                async move |update: v2::UpdateSessionNotification, _: V2ConnectionTo<Agent>| {
                    updates
                        .try_send(Event::Update(update))
                        .map_err(Error::into_internal_error)
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .on_receive_request(
                async move |request: v2::RequestPermissionRequest,
                            responder: Responder<v2::RequestPermissionResponse>,
                            _: V2ConnectionTo<Agent>| {
                    permissions
                        .try_send(Event::Permission(request, responder))
                        .map_err(Error::into_internal_error)
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |request: v2::CreateElicitationRequest,
                            responder: Responder<v2::CreateElicitationResponse>,
                            _: V2ConnectionTo<Agent>| {
                    forms_tx
                        .try_send(Event::Form(request, responder))
                        .map_err(Error::into_internal_error)
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(
                HttpClient::new(format!("{scheme}://{}", self.address))
                    .map_err(Error::into_internal_error)?,
                async move |cx| {
                    let mut capabilities = v2::ClientCapabilities::new();
                    if forms {
                        capabilities = capabilities.elicitation(
                            v2::ElicitationCapabilities::new()
                                .form(v2::ElicitationFormCapabilities::new()),
                        );
                    }
                    let initialized = cx
                        .send_request(
                            v2::InitializeRequest::new(
                                ProtocolVersion::V2,
                                v2::Implementation::new("hive-bee-tests", "1"),
                            )
                            .capabilities(capabilities),
                        )
                        .block_task()
                        .await?;
                    assert_eq!(initialized.protocol_version, ProtocolVersion::V2);
                    flow(Peer {
                        cx,
                        rx,
                        boundary: tx,
                        recent: Default::default(),
                    })
                    .await
                },
            )
            .await
    }

    pub fn journal(
        &self,
        id: &v2::SessionId,
        key: &str,
        receipt: &v2::PromptResponse,
        terminal: &str,
    ) {
        // Terminal idle is published after this commit, so no polling is needed.
        let db = rusqlite::Connection::open_with_flags(
            &self.database,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let (content, fingerprint, result, saved_terminal): (String, String, String, String) = db.query_row(
            "SELECT content,content_fingerprint,result,terminal FROM prompts WHERE session_id=?1 AND key=?2",
            rusqlite::params![id.to_string(), key],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        ).unwrap();
        assert!(content.is_empty());
        assert!(fingerprint.starts_with("sha256-v1:"));
        let result: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(result["status"], "acknowledged");
        assert_eq!(result["messageId"], receipt.message_id.to_string());
        assert_eq!(result.as_object().unwrap().len(), 2);
        assert_eq!(saved_terminal, terminal);
    }
}

pub fn controlled(meta: &Option<v2::Meta>, key: &str) -> v2::Meta {
    let mut meta = meta.clone().expect("Hive must provide control metadata");
    meta.get_mut("hive").unwrap()["key"] = key.into();
    meta
}

impl Peer {
    pub async fn open(&self, cwd: PathBuf) -> Result<v2::NewSessionResponse> {
        self.cx
            .send_request(v2::NewSessionRequest::new(cwd))
            .block_task()
            .await
    }

    pub async fn prompt(
        &self,
        opened: &v2::NewSessionResponse,
        key: &str,
    ) -> Result<v2::PromptResponse> {
        self.cx
            .send_request(
                v2::PromptRequest::new(opened.session_id.clone(), vec!["Run the fixture".into()])
                    .meta(controlled(&opened.meta, key)),
            )
            .block_task()
            .await
    }

    pub async fn event(&mut self) -> Event {
        let event = timeout(Duration::from_secs(10), self.rx.recv())
            .await
            .unwrap_or_else(|_| panic!("Bee event deadline; recent safe trace: {:?}", self.recent))
            .expect("ACP event channel closed");
        if let Event::Update(update) = &event {
            let value = serde_json::to_value(update).unwrap();
            // Only protocol discriminators and sequence numbers enter failure logs, never ownership tokens.
            self.recent.push_back(format!(
                "{} seq={}",
                value["update"]["sessionUpdate"], value["_meta"]["hive"]["seq"]
            ));
            if self.recent.len() > 20 {
                self.recent.pop_front();
            }
        }
        event
    }

    pub async fn update(&mut self) -> Value {
        match self.event().await {
            Event::Update(update) => serde_json::to_value(update).unwrap(),
            _ => panic!(
                "Unexpected interaction or response boundary; recent safe trace: {:?}",
                self.recent
            ),
        }
    }

    pub async fn idle(&mut self, stop: &str) -> Vec<Value> {
        let mut events = Vec::new();
        loop {
            let event = self.update().await;
            // Attachments broadcast a known-state barrier without a stop reason; it is not another completion.
            let done = event["update"]["sessionUpdate"] == "state_update"
                && event["update"]["state"] == "idle"
                && event["update"]["stopReason"].is_string();
            if done {
                assert_eq!(event["update"]["stopReason"], stop);
            }
            events.push(event);
            assert!(
                events.len() <= 256,
                "Bee turn exceeded bounded fixture event budget"
            );
            if done {
                return events;
            }
        }
    }

    pub async fn ready(&mut self) {
        loop {
            let update = self.update().await;
            if update["update"]["sessionUpdate"] == "agent_message_chunk" {
                assert_eq!(
                    update["update"]["content"]["text"],
                    "Ready for cancellation."
                );
                return;
            }
        }
    }

    pub async fn attach(
        &mut self,
        id: v2::SessionId,
        cwd: PathBuf,
        replay: Option<v2::ReplayFrom>,
        observer: bool,
    ) -> Result<(v2::ResumeSessionResponse, Vec<Value>)> {
        let mut request = v2::ResumeSessionRequest::new(id, cwd);
        request.replay_from = replay;
        if observer {
            request.meta = Some(v2::Meta::from_iter([(
                "hive".into(),
                json!({"mode":"observe"}),
            )]));
        }
        let boundary = self.boundary.clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.cx
            .prepare_request(request)
            .on_receiving_result(async move |result| {
                boundary
                    .try_send(Event::Boundary)
                    .map_err(Error::into_internal_error)?;
                let _ = tx.send(result);
                Ok(())
            })?;
        let result = rx.await.map_err(Error::into_internal_error)??;
        Ok((result, self.batch().await))
    }

    pub async fn close(&mut self, opened: &v2::NewSessionResponse) -> Result<Vec<Value>> {
        let boundary = self.boundary.clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.cx
            .prepare_request(
                v2::CloseSessionRequest::new(opened.session_id.clone()).meta(opened.meta.clone()),
            )
            .on_receiving_result(async move |result| {
                boundary
                    .try_send(Event::Boundary)
                    .map_err(Error::into_internal_error)?;
                let _ = tx.send(result);
                Ok(())
            })?;
        rx.await.map_err(Error::into_internal_error)??;
        Ok(self.batch().await)
    }

    async fn batch(&mut self) -> Vec<Value> {
        let mut events = Vec::new();
        loop {
            match self.event().await {
                Event::Boundary => return events,
                Event::Update(update) => events.push(serde_json::to_value(update).unwrap()),
                _ => panic!("Unexpected interaction during attachment"),
            }
            assert!(
                events.len() <= 256,
                "Bee replay exceeded bounded fixture event budget"
            );
        }
    }
}

pub fn native(events: &[Value], kind: &str) -> Vec<Value> {
    events
        .iter()
        .filter(|event| event["update"]["sessionUpdate"] == kind)
        .map(|event| event["update"].clone())
        .collect()
}

pub fn text(events: &[Value]) -> String {
    native(events, "agent_message_chunk")
        .iter()
        .map(|event| event["content"]["text"].as_str().unwrap())
        .collect()
}
