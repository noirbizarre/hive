//! Loopback ACPv2 gateway with a shared, in-memory session broker.

use std::{
    collections::{HashMap, VecDeque},
    fs::{File, OpenOptions},
    future::IntoFuture,
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use agent_client_protocol::schema::{ProtocolVersion, v1, v2};
use agent_client_protocol::{
    AcpAgent, AcpAgentConfig, Agent, Client, ConnectTo, ConnectionTo, Error, Responder,
    V2ConnectionTo,
};
use agent_client_protocol_http::{CorsOptions, HttpClient, ServerOptions};
use axum::{Json, Router, response::Html, routing::get};
use fs2::FileExt;
use hive_core::SessionId;
use hive_protocol::{AcpHttpServer, AgentVersion};
use serde::Serialize;
use tokio::sync::{mpsc, oneshot};

pub mod config;
use config::{Config, Profile};

enum Downstream {
    Stdio(AcpAgent),
    Network(HttpClient),
}

impl ConnectTo<Client> for Downstream {
    async fn connect_to(self, client: impl ConnectTo<Agent>) -> agent_client_protocol::Result<()> {
        match self {
            Self::Stdio(agent) => ConnectTo::<Client>::connect_to(agent, client).await,
            Self::Network(agent) => ConnectTo::<Client>::connect_to(agent, client).await,
        }
    }
}

fn transport(profile: &Profile) -> Result<Downstream, String> {
    if let Some(command) = &profile.command {
        let configuration = AcpAgentConfig::new(command)
            .args(profile.args.clone())
            .envs(profile.env.clone());
        return Ok(Downstream::Stdio(AcpAgent::new(configuration)));
    }
    let endpoint = profile
        .endpoint
        .as_deref()
        .ok_or("missing agent endpoint")?;
    let mut headers = reqwest::header::HeaderMap::new();
    for (name, value) in &profile.headers {
        let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| "invalid agent header name")?;
        let value = reqwest::header::HeaderValue::from_str(value)
            .map_err(|_| "invalid agent header value")?;
        headers.insert(name, value);
    }
    // Never include endpoint URLs or authentication headers in errors: they may contain credentials.
    HttpClient::builder(endpoint)
        .configure_http(|http| http.default_headers(headers))
        .build()
        .map(Downstream::Network)
        .map_err(|_| "invalid agent endpoint".into())
}

#[derive(Clone)]
pub struct Broker {
    registry: Arc<Config>,
    sessions: Arc<Mutex<HashMap<String, Entry>>>,
}

#[derive(Clone)]
struct Entry {
    cwd: v2::AbsolutePath,
    profile: String,
    abort: tokio::task::AbortHandle,
    commands: mpsc::UnboundedSender<Command>,
    // Replay is intentionally limited to retained snapshots, not raw chunks.
    history: Arc<Mutex<History>>,
    gateway: Arc<Mutex<V2ConnectionTo<Client>>>,
}

#[derive(Default)]
struct History {
    snapshots: VecDeque<v2::SessionUpdate>,
    truncated: bool,
}

enum Command {
    Prompt(
        v2::PromptRequest,
        oneshot::Sender<Result<v2::PromptResponse, String>>,
    ),
    Cancel,
    Close(oneshot::Sender<Result<(), String>>),
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub name: &'static str,
    pub version: &'static str,
    pub sessions: usize,
    pub default_agent: Option<String>,
    pub active_by_agent: HashMap<String, usize>,
}

impl Broker {
    #[must_use]
    pub fn new(registry: Config) -> Self {
        Self {
            registry: Arc::new(registry),
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn entry(&self, id: &v2::SessionId) -> agent_client_protocol::Result<Entry> {
        self.sessions
            .lock()
            .map_err(|_| Error::internal_error())?
            .get(&id.to_string())
            .cloned()
            .ok_or_else(|| Error::invalid_params().data(format!("unknown Hive session {id}")))
    }

    fn status(&self) -> Status {
        let sessions = self.sessions.lock();
        let mut active_by_agent = HashMap::new();
        if let Ok(sessions) = &sessions {
            for entry in sessions.values() {
                *active_by_agent.entry(entry.profile.clone()).or_insert(0) += 1;
            }
        }
        Status {
            name: "hive",
            version: env!("CARGO_PKG_VERSION"),
            sessions: sessions.map_or(0, |sessions| sessions.len()),
            default_agent: self.registry.default_agent.clone(),
            active_by_agent,
        }
    }

    fn provider_info(&self) -> serde_json::Value {
        serde_json::json!({ "defaultAgent": self.registry.default_agent, "agents": self.registry.public_profiles() })
    }

    async fn shutdown(&self) {
        let entries = self
            .sessions
            .lock()
            .map(|mut sessions| sessions.drain().map(|(_, entry)| entry).collect::<Vec<_>>())
            .unwrap_or_default();
        for entry in entries {
            let (reply, received) = oneshot::channel();
            let _ = entry.commands.send(Command::Cancel);
            if entry.commands.send(Command::Close(reply)).is_ok() {
                let _ = tokio::time::timeout(std::time::Duration::from_secs(2), received).await;
            }
            // Graceful close is best-effort; dropping the SDK connection terminates its stdio process group.
            entry.abort.abort();
        }
    }

    async fn create(
        &self,
        mut request: v2::NewSessionRequest,
        gateway: V2ConnectionTo<Client>,
    ) -> agent_client_protocol::Result<v2::NewSessionResponse> {
        let requested = request.meta.as_mut().and_then(|meta| meta.get_mut("hive"));
        let chosen = match requested {
            Some(serde_json::Value::Object(hive)) => {
                let id = match hive.remove("agent") {
                    Some(serde_json::Value::String(id)) if !id.is_empty() => id,
                    _ => return Err(Error::invalid_params().data(
                        "_meta.hive.agent must be a nonempty profile ID; see GET /hive/v1/providers",
                    )),
                };
                if hive.is_empty() {
                    request
                        .meta
                        .as_mut()
                        .expect("metadata exists")
                        .remove("hive");
                }
                Some(id)
            }
            Some(_) => {
                return Err(
                    Error::invalid_params().data("_meta.hive must be an object containing agent")
                );
            }
            None => self.registry.default_agent.clone(),
        };
        let chosen = chosen.ok_or_else(|| Error::invalid_params().data("no default agent configured; set default_agent in Hive config or pass _meta.hive.agent in session/new"))?;
        let provider = self.registry.agents.get(&chosen).cloned().ok_or_else(|| {
            Error::invalid_params().data(format!(
                "unknown agent profile '{chosen}'; query GET /hive/v1/providers"
            ))
        })?;
        let hive_id = v2::SessionId::new(SessionId::new().as_str());
        let (tx, rx) = mpsc::unbounded_channel();
        let (ready_tx, ready_rx) = oneshot::channel();
        let (active_tx, active_rx) = oneshot::channel();
        let history = Arc::new(Mutex::new(History::default()));
        let current_gateway = Arc::new(Mutex::new(gateway));
        let cwd = request.cwd.clone();
        let entry_history = history.clone();
        let entry_gateway = current_gateway.clone();
        let id = hive_id.clone();
        let sessions = self.sessions.clone();
        // Each session owns its subprocess: no guessed transport support or duplicate probing.
        let task = tokio::spawn(async move {
            if provider.preferred_version() == AgentVersion::V1 {
                run_v1_agent(
                    provider,
                    request,
                    id.clone(),
                    rx,
                    (ready_tx, active_rx),
                    history,
                    current_gateway,
                )
                .await;
            } else {
                run_agent(
                    provider,
                    request,
                    id.clone(),
                    rx,
                    (ready_tx, active_rx),
                    history,
                    current_gateway,
                )
                .await;
            }
            // A crashed or closed subprocess must not remain advertised as active.
            if let Ok(mut sessions) = sessions.lock() {
                sessions.remove(&id.to_string());
            }
        });
        let entry = Entry {
            cwd,
            profile: chosen,
            abort: task.abort_handle(),
            commands: tx,
            history: entry_history,
            gateway: entry_gateway,
        };
        match tokio::time::timeout(std::time::Duration::from_secs(15), ready_rx).await {
            Ok(Ok(Ok(mut response))) => {
                self.sessions
                    .lock()
                    .map_err(|_| Error::internal_error())?
                    .insert(hive_id.to_string(), entry);
                let _ = active_tx.send(());
                response.session_id = hive_id;
                Ok(response)
            }
            Ok(Ok(Err(message))) => {
                task.abort();
                Err(Error::internal_error().data(message))
            }
            Ok(Err(_)) => {
                task.abort();
                Err(Error::internal_error()
                    .data("agent exited during session startup; check its command and stderr"))
            }
            Err(_) => {
                task.abort();
                Err(Error::internal_error()
                    .data("agent startup timed out; check the profile endpoint or command"))
            }
        }
    }
}

fn forward(
    history: &Mutex<History>,
    gateway: &Mutex<V2ConnectionTo<Client>>,
    id: &v2::SessionId,
    update: v2::SessionUpdate,
) -> agent_client_protocol::Result<()> {
    if matches!(
        update,
        v2::SessionUpdate::AgentMessage(_) | v2::SessionUpdate::UserMessage(_)
    ) {
        let mut history = history.lock().map_err(|_| Error::internal_error())?;
        if history.snapshots.len() == 256 {
            history.snapshots.pop_front();
            history.truncated = true;
        }
        history.snapshots.push_back(update.clone());
    }
    let sent = gateway
        .lock()
        .map_err(|_| Error::internal_error())?
        .send_notification(v2::UpdateSessionNotification::new(id.clone(), update));
    // A detached client may resume later; its WebSocket/SSE send failure must not kill the agent.
    if sent.is_err() {
        tracing::debug!("client detached before session update; retaining the agent session");
    }
    Ok(())
}

async fn run_agent(
    provider: Profile,
    request: v2::NewSessionRequest,
    hive_id: v2::SessionId,
    mut rx: mpsc::UnboundedReceiver<Command>,
    startup: (
        oneshot::Sender<Result<v2::NewSessionResponse, String>>,
        oneshot::Receiver<()>,
    ),
    history: Arc<Mutex<History>>,
    gateway: Arc<Mutex<V2ConnectionTo<Client>>>,
) {
    let (ready, active) = startup;
    let agent = match transport(&provider) {
        Ok(agent) => agent,
        Err(error) => {
            let _ = ready.send(Err(format!("invalid provider command: {error}")));
            return;
        }
    };
    let downstream = Arc::new(Mutex::new(None::<v2::SessionId>));
    let route = downstream.clone();
    let result = Client
        .v2()
        .on_receive_notification(
            {
                let gateway = gateway.clone();
                let history = history.clone();
                let hive_id = hive_id.clone();
                async move |notification: v2::UpdateSessionNotification,
                            _cx: V2ConnectionTo<Agent>| {
                    if route.lock().map_err(|_| Error::internal_error())?.as_ref()
                        != Some(&notification.session_id)
                    {
                        return Ok(());
                    }
                    forward(&history, &gateway, &hive_id, notification.update)
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async |_request: v2::RequestPermissionRequest,
                   responder: Responder<v2::RequestPermissionResponse>,
                   _cx: V2ConnectionTo<Agent>| {
                // Noninteractive bootstrap cannot safely grant agent permissions.
                responder.respond(v2::RequestPermissionResponse::new(
                    v2::RequestPermissionOutcome::Cancelled,
                ))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(agent, async move |cx| {
            let initialized = cx
                .send_request(v2::InitializeRequest::new(
                    ProtocolVersion::V2,
                    v2::Implementation::new("hive", env!("CARGO_PKG_VERSION")),
                ))
                .block_task()
                .await;
            match initialized {
                Ok(response) if response.protocol_version == ProtocolVersion::V2 && response.capabilities.session.is_some() => {}
                Ok(_) => {
                    let _ =
                        ready.send(Err("agent does not advertise ACPv2 session support".into()));
                    return Ok(());
                }
                Err(error) => {
                    tracing::warn!(code = ?error.code, "agent initialization failed");
                    let _ = ready.send(Err("agent initialization failed; check its protocol version and availability".into()));
                    return Ok(());
                }
            }
            let opened = cx.send_request(request).block_task().await;
            let opened = match opened {
                Ok(response) => response,
                Err(error) => {
                    tracing::warn!(code = ?error.code, "agent session/new failed");
                    let _ = ready.send(Err("agent session/new failed; check its capabilities and configuration".into()));
                    return Ok(());
                }
            };
            let downstream_id = opened.session_id.clone();
            if let Ok(mut route) = downstream.lock() {
                *route = Some(downstream_id.clone());
            }
            let _ = ready.send(Ok(opened));
            if active.await.is_err() { return Ok(()); }
            while let Some(command) = rx.recv().await {
                match command {
                    Command::Prompt(mut request, reply) => {
                        request.session_id = downstream_id.clone();
                        let prompt = cx.send_request(request).block_task();
                        tokio::pin!(prompt);
                        let result = loop {
                            tokio::select! {
                                result = &mut prompt => break result,
                                incoming = rx.recv() => match incoming {
                                    Some(Command::Cancel) => { let _ = cx.send_notification(v2::CancelSessionNotification::new(downstream_id.clone())); }
                                    Some(Command::Prompt(_, reply)) => { let _ = reply.send(Err("session already has foreground work".into())); }
                                    Some(Command::Close(reply)) => {
                                        let _ = cx.send_notification(v2::CancelSessionNotification::new(downstream_id.clone()));
                                        let _ = reply.send(Err("cancel active work before closing".into()));
                                    }
                                    None => return Ok(()),
                                }
                            }
                        }.map_err(|_| "agent prompt failed; check its availability".to_string());
                        let _ = reply.send(result);
                    }
                    Command::Cancel => {
                        let _ = cx.send_notification(v2::CancelSessionNotification::new(
                            downstream_id.clone(),
                        ));
                    }
                    Command::Close(reply) => {
                        let result = cx
                            .send_request(v2::CloseSessionRequest::new(downstream_id.clone()))
                            .block_task()
                            .await
                            .map(|_| ())
                            .map_err(|_| "agent close failed; check its availability".to_string());
                        let _ = reply.send(result);
                        break;
                    }
                }
            }
            Ok(())
        })
        .await;
    if let Err(error) = result {
        tracing::warn!(code = ?error.code, "downstream agent connection closed");
    }
}

/// ACPv1 prompt completion is a response, unlike v2's independent idle update.
async fn run_v1_agent(
    provider: Profile,
    request: v2::NewSessionRequest,
    hive_id: v2::SessionId,
    mut rx: mpsc::UnboundedReceiver<Command>,
    startup: (
        oneshot::Sender<Result<v2::NewSessionResponse, String>>,
        oneshot::Receiver<()>,
    ),
    history: Arc<Mutex<History>>,
    gateway: Arc<Mutex<V2ConnectionTo<Client>>>,
) {
    let (ready, active) = startup;
    let agent = match transport(&provider) {
        Ok(agent) => agent,
        Err(error) => {
            let _ = ready.send(Err(format!("invalid provider command: {error}")));
            return;
        }
    };
    let route = Arc::new(Mutex::new(None::<v1::SessionId>));
    let notifications = route.clone();
    let message_id = Arc::new(Mutex::new(None::<v2::MessageId>));
    let chunk_message_id = message_id.clone();
    let result = Client
        .builder()
        .on_receive_notification(
            {
                let gateway = gateway.clone();
                let hive_id = hive_id.clone();
                async move |notification: v1::SessionNotification, _cx: ConnectionTo<Agent>| {
                    if notifications
                        .lock()
                        .map_err(|_| Error::internal_error())?
                        .as_ref()
                        != Some(&notification.session_id)
                    {
                        return Ok(());
                    }
                    // V1 chunks have no message ID: assign one per Hive prompt.
                    if let v1::SessionUpdate::AgentMessageChunk(chunk) = notification.update
                    && let v1::ContentBlock::Text(text) = chunk.content
                {
                    let id = chunk_message_id.lock().map_err(|_| Error::internal_error())?.clone().ok_or_else(Error::internal_error)?;
                    let update = v2::SessionUpdate::AgentMessageChunk(v2::ContentChunk::new(
                        text.text.into(),
                        id,
                        ));
                        let _ = gateway
                            .lock()
                            .map_err(|_| Error::internal_error())?
                            .send_notification(v2::UpdateSessionNotification::new(
                                hive_id.clone(),
                                update,
                            ));
                    }
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(agent, async move |cx| {
            let initialized = cx
                .send_request(v1::InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await;
            match initialized {
                Ok(response) if response.protocol_version == ProtocolVersion::V1 => {}
                Ok(_) => {
                    let _ = ready.send(Err("agent negotiated a different protocol version; correct the profile's versions setting".into()));
                    return Ok(());
                }
                Err(error) => {
                    tracing::warn!(code = ?error.code, "v1 initialize failed");
                    let _ = ready.send(Err("v1 initialize failed; check the declared protocol version and agent availability".into()));
                    return Ok(());
                }
            }
            if !request.mcp_servers.is_empty() {
                let _ = ready.send(Err(
                    "v1 adapter cannot translate v2 MCP server declarations".into(),
                ));
                return Ok(());
            }
            let opened = cx
                .send_request(v1::NewSessionRequest::new(request.cwd.0))
                .block_task()
                .await;
            let downstream_id = match opened {
                Ok(response) => response.session_id,
                Err(error) => {
                    tracing::warn!(code = ?error.code, "v1 session/new failed");
                    let _ = ready.send(Err("v1 session/new failed; check agent availability".into()));
                    return Ok(());
                }
            };
            if let Ok(mut route) = route.lock() {
                *route = Some(downstream_id.clone());
            }
            let _ = ready.send(Ok(v2::NewSessionResponse::new(hive_id.clone())));
            if active.await.is_err() { return Ok(()); }
            while let Some(command) = rx.recv().await {
                match command {
                    Command::Prompt(request, reply) => {
                        let id = v2::MessageId::new(format!("hive-v1-{}", uuid::Uuid::new_v4()));
                        let blocks = request
                            .prompt
                            .iter()
                            .map(|block| match block {
                                v2::ContentBlock::Text(text) => Ok(v1::ContentBlock::Text(
                                    v1::TextContent::new(text.text.clone()),
                                )),
                                _ => Err("v1 adapter supports text prompts only"),
                            })
                            .collect::<Result<Vec<_>, _>>();
                        let Ok(blocks) = blocks else {
                            let _ = reply
                                .send(Err("unsupported v2 prompt content for v1 agent".into()));
                            continue;
                        };
                        if let Ok(mut current) = message_id.lock() { *current = Some(v2::MessageId::new(format!("v1-agent-{}", uuid::Uuid::new_v4()))); }
                        // Acknowledgement precedes v1 turn completion; the task then emits idle.
                        let _ = reply.send(Ok(v2::PromptResponse::new(id.clone())));
                        let user = v2::SessionUpdate::UserMessage(
                            v2::UserMessage::new(id).content(request.prompt),
                        );
                        let _ = forward(&history, &gateway, &hive_id, user);
                        let _ = forward(
                            &history,
                            &gateway,
                            &hive_id,
                            v2::SessionUpdate::StateUpdate(v2::StateUpdate::Running(
                                v2::RunningStateUpdate::new(),
                            )),
                        );
                        let response = cx.send_request(v1::PromptRequest::new(downstream_id.clone(), blocks)).block_task();
                        tokio::pin!(response);
                        // V1 prompt waits for completion, so cancellation must remain live on the other lane.
                        let result = loop {
                            tokio::select! {
                                result = &mut response => break result,
                                command = rx.recv() => match command {
                                    Some(Command::Cancel) => { let _ = cx.send_notification(v1::CancelNotification::new(downstream_id.clone())); }
                                    Some(Command::Prompt(_, reply)) => { let _ = reply.send(Err("session already has foreground work".into())); }
                                    Some(Command::Close(reply)) => { let _ = cx.send_notification(v1::CancelNotification::new(downstream_id.clone())); let _ = reply.send(Err("cancel active work before closing".into())); }
                                    None => break Err(Error::internal_error().data("client disconnected")),
                                }
                            }
                        };
                        let stop = match result {
                            Ok(response) => match response.stop_reason {
                                v1::StopReason::EndTurn => v2::StopReason::EndTurn,
                                _ => v2::StopReason::Other("_v1_stop".into()),
                            },
                            Err(error) => {
                                tracing::warn!(%error, "v1 prompt failed");
                                v2::StopReason::Other("_v1_error".into())
                            }
                        };
                        let _ = forward(
                            &history,
                            &gateway,
                            &hive_id,
                            v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(
                                v2::IdleStateUpdate::new().stop_reason(stop),
                            )),
                        );
                    }
                    Command::Cancel => {
                        let _ = cx
                            .send_notification(v1::CancelNotification::new(downstream_id.clone()));
                    }
                    Command::Close(reply) => {
                        let _ = reply.send(Ok(()));
                        break;
                    }
                }
            }
            Ok(())
        })
        .await;
    if let Err(error) = result {
        tracing::warn!(code = ?error.code, "v1 agent connection closed");
    }
}

pub fn router(broker: Broker) -> Router {
    router_with_origin(broker, None)
}

/// Only the specified same-origin browser UI may initiate a WebSocket upgrade.
pub fn router_with_origin(broker: Broker, origin: Option<&str>) -> Router {
    let state = broker.clone();
    let agent = move || gateway_agent(state.clone());
    let mut server = AcpHttpServer::new(agent);
    if let Some(origin) = origin {
        // The SDK otherwise rejects browser WS upgrades that carry an Origin header.
        let cors = CorsOptions::allow_origins([origin])
            .expect("a loopback socket origin is a valid header");
        server = server.with_options(ServerOptions::default().with_cors(cors));
    }
    server
        .into_router()
        .route(
            "/hive/v1/health",
            get(|| async { Json(serde_json::json!({"status":"ok"})) }),
        )
        .route(
            "/hive/v1/ready",
            get(|| async { Json(serde_json::json!({"ready":true})) }),
        )
        .route(
            "/hive/v1/status",
            get({
                let broker = broker.clone();
                move || {
                    let broker = broker.clone();
                    async move { Json(broker.status()) }
                }
            }),
        )
        .route(
            "/hive/v1/providers",
            get({
                let broker = broker.clone();
                move || {
                    let broker = broker.clone();
                    async move { Json(broker.provider_info()) }
                }
            }),
        )
        .route(
            "/hive/v1/web",
            get(|| async { Html(include_str!("../../../clients/hive-web/index.html")) }),
        )
        .route(
            "/hive/v1/web/app.js",
            get(|| async {
                (
                    [(axum::http::header::CONTENT_TYPE, "text/javascript")],
                    include_str!("../../../clients/hive-web/app.js"),
                )
            }),
        )
}

fn gateway_agent(broker: Broker) -> impl agent_client_protocol::ConnectTo<Client> {
    Agent
        .v2()
        .name("hive")
        .on_receive_request(
            async |request: v2::InitializeRequest,
                   responder: Responder<v2::InitializeResponse>,
                   _cx: V2ConnectionTo<Client>| {
                responder.respond(
                    v2::InitializeResponse::new(
                        request.protocol_version,
                        v2::Implementation::new("hive", env!("CARGO_PKG_VERSION")),
                    )
                    .capabilities(
                        v2::AgentCapabilities::new().session(v2::SessionCapabilities::new()),
                    ),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let broker = broker.clone();
                async move |request: v2::NewSessionRequest,
                            responder: Responder<v2::NewSessionResponse>,
                            cx: V2ConnectionTo<Client>| {
                    match broker.create(request, cx).await {
                        Ok(response) => responder.respond(response),
                        Err(error) => responder.respond_with_error(error),
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let broker = broker.clone();
                async move |request: v2::PromptRequest,
                            responder: Responder<v2::PromptResponse>,
                            _cx: V2ConnectionTo<Client>| {
                    let entry = broker.entry(&request.session_id)?;
                    let (reply, received) = oneshot::channel();
                    entry
                        .commands
                        .send(Command::Prompt(request, reply))
                        .map_err(|_| {
                            Error::internal_error()
                                .data("agent disconnected; create another session")
                        })?;
                    match received.await {
                        Ok(Ok(response)) => responder.respond(response),
                        Ok(Err(error)) => {
                            responder.respond_with_error(Error::internal_error().data(error))
                        }
                        Err(_) => responder.respond_with_error(
                            Error::internal_error().data("agent disconnected during prompt"),
                        ),
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let broker = broker.clone();
                async move |request: v2::ListSessionsRequest,
                            responder: Responder<v2::ListSessionsResponse>,
                            _cx: V2ConnectionTo<Client>| {
                    let entries = broker
                        .sessions
                        .lock()
                        .map_err(|_| Error::internal_error())?
                        .iter()
                        .filter(|(_, entry)| {
                            request.cwd.as_ref().is_none_or(|cwd| cwd == &entry.cwd)
                        })
                        .map(|(id, entry)| {
                            v2::SessionInfo::new(v2::SessionId::new(id.clone()), entry.cwd.clone())
                        })
                        .collect();
                    responder.respond(v2::ListSessionsResponse::new(entries))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let broker = broker.clone();
                async move |request: v2::ResumeSessionRequest,
                            responder: Responder<v2::ResumeSessionResponse>,
                            cx: V2ConnectionTo<Client>| {
                    let entry = broker.entry(&request.session_id)?;
                    if entry.cwd != request.cwd {
                        return responder.respond_with_error(
                            Error::invalid_params().data("session cwd does not match"),
                        );
                    }
                    if matches!(request.replay_from, Some(v2::ReplayFrom::Other(_))) {
                        return responder.respond_with_error(Error::invalid_params().data("unsupported replay cursor; use replayFrom: {type: start}"));
                    }
                    // Replay notifications must be sent before the resume response.
                    *entry.gateway.lock().map_err(|_| Error::internal_error())? = cx.clone();
                    if request.replay_from.is_some() {
                        let history = entry.history.lock().map_err(|_| Error::internal_error())?;
                        if history.truncated {
                            return responder.respond_with_error(Error::invalid_request().data("replay history was truncated; resume without replay or create a new session"));
                        }
                        for update in history.snapshots.iter().cloned() {
                            cx.send_notification(v2::UpdateSessionNotification::new(
                                request.session_id.clone(),
                                update,
                            ))?;
                        }
                    }
                    responder.respond(v2::ResumeSessionResponse::new())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let broker = broker.clone();
                async move |request: v2::CloseSessionRequest,
                            responder: Responder<v2::CloseSessionResponse>,
                            _cx: V2ConnectionTo<Client>| {
                    let entry = broker.entry(&request.session_id)?;
                    let (reply, received) = oneshot::channel();
                    entry
                        .commands
                        .send(Command::Close(reply))
                        .map_err(|_| Error::internal_error())?;
                    match received.await {
                        Ok(Ok(())) => {
                            broker
                                .sessions
                                .lock()
                                .map_err(|_| Error::internal_error())?
                                .remove(&request.session_id.to_string());
                            responder.respond(v2::CloseSessionResponse::new())
                        }
                        _ => responder.respond_with_error(
                            Error::internal_error().data("agent could not close session"),
                        ),
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |request: v2::CancelSessionNotification, _cx: V2ConnectionTo<Client>| {
                let entry = broker.entry(&request.session_id)?;
                entry.commands.send(Command::Cancel).map_err(|_| {
                    Error::internal_error().data("agent disconnected during cancellation")
                })
            },
            agent_client_protocol::on_receive_notification!(),
        )
}

/// Reject remote binds: this initial gateway has no authentication or TLS.
pub async fn serve(addr: SocketAddr, registry: Config) -> std::io::Result<()> {
    if !addr.ip().is_loopback() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "remote binding requires authentication; use a loopback address",
        ));
    }
    let _instance = instance_lock()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(address = %listener.local_addr()?, "Hive listening");
    let origin = format!("http://{}", listener.local_addr()?);
    let broker = Broker::new(registry);
    let (shutdown_done, shut_down) = oneshot::channel();
    let server = axum::serve(listener, router_with_origin(broker.clone(), Some(&origin)))
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            broker.shutdown().await;
            let _ = shutdown_done.send(());
        })
        .into_future();
    tokio::pin!(server);
    tokio::select! {
        result = &mut server => result,
        _ = shut_down => tokio::time::timeout(std::time::Duration::from_secs(8), &mut server).await
            .unwrap_or_else(|_| Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "Hive HTTP shutdown timed out; active clients may need to reconnect"))),
    }
}

fn instance_lock() -> std::io::Result<File> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .ok_or_else(|| {
            std::io::Error::other("set XDG_RUNTIME_DIR or HOME to locate the Hive lock")
        })?;
    let dir = base.join("hive");
    std::fs::create_dir_all(&base)?;
    if !dir.exists() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&dir)
                .or_else(|error| {
                    if error.kind() == std::io::ErrorKind::AlreadyExists {
                        Ok(())
                    } else {
                        Err(error)
                    }
                })?;
        }
        #[cfg(not(unix))]
        std::fs::create_dir(&dir)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata = std::fs::symlink_metadata(&dir)?;
        if !metadata.is_dir() || metadata.permissions().mode() & 0o077 != 0 {
            return Err(std::io::Error::other(format!(
                "insecure Hive runtime directory {}; use a private directory (mode 0700)",
                dir.display()
            )));
        }
    }
    let path = dir.join("daemon.lock");
    if std::fs::symlink_metadata(&path).is_ok_and(|meta| meta.file_type().is_symlink()) {
        return Err(std::io::Error::other(
            "Hive lock is a symlink; remove it after checking the runtime directory",
        ));
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let lock = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = lock.metadata()?;
        if metadata.nlink() != 1
            || metadata.mode() & 0o077 != 0
            || metadata.uid() != std::fs::metadata(&dir)?.uid()
        {
            return Err(std::io::Error::other(
                "insecure Hive lock file; check its owner, links and mode",
            ));
        }
    }
    lock.try_lock_exclusive().map_err(|error| {
        if error.kind() == std::io::ErrorKind::WouldBlock {
            std::io::Error::new(
                error.kind(),
                "Hive is already running for this user; use `hive status` to inspect it",
            )
        } else {
            error
        }
    })?;
    Ok(lock)
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        if let Ok(mut terminate) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}
