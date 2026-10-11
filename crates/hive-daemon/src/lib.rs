//! Loopback ACPv2 gateway with a shared, in-memory session broker.

use std::{
    collections::{BTreeMap, HashMap, VecDeque},
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
use tokio::sync::{mpsc, oneshot, watch};

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
    control: Arc<Mutex<Control>>,
}

struct Control {
    generation: u64,
    token: String,
    pilot: Option<String>,
    subscribers: HashMap<String, V2ConnectionTo<Client>>,
    changed: watch::Sender<u64>,
    next_seq: u64,
    events: VecDeque<(u64, v2::SessionUpdate)>,
    running: bool,
    closing: bool,
    prompts: HashMap<String, PromptRecord>,
}

struct PromptRecord {
    content: Vec<v2::ContentBlock>,
    result: Option<Result<v2::PromptResponse, String>>,
    waiters: Vec<oneshot::Sender<Result<v2::PromptResponse, String>>>,
}

enum Command {
    Prompt(v2::PromptRequest, u64, String),
    Cancel(u64),
    Close(u64, oneshot::Sender<Result<(), String>>),
}

impl Control {
    fn new(client_id: String, gateway: V2ConnectionTo<Client>) -> Self {
        let (changed, _) = watch::channel(1);
        Self {
            generation: 1,
            token: uuid::Uuid::new_v4().to_string(),
            pilot: Some(client_id.clone()),
            subscribers: HashMap::from([(client_id, gateway)]),
            changed,
            next_seq: 1,
            events: VecDeque::new(),
            running: false,
            closing: false,
            prompts: HashMap::new(),
        }
    }

    fn metadata(&self, client_id: &str) -> v2::Meta {
        let mut meta = v2::Meta::new();
        let mut hive = serde_json::json!({
            "generation": self.generation, "nextSeq": self.next_seq,
            "running": self.running, "pilot": self.pilot.is_some(),
            "role": if self.pilot.as_deref() == Some(client_id) { "pilot" } else { "observer" },
        });
        // Observers must never receive the credential that authorizes mutations.
        if self.pilot.as_deref() == Some(client_id) {
            hive["token"] = serde_json::json!(self.token);
        }
        meta.insert("hive".into(), hive);
        meta
    }

    fn status_update(&self) -> v2::SessionUpdate {
        v2::SessionUpdate::Other(v2::OtherSessionUpdate::new(
            "_hive_control",
            BTreeMap::from([
                ("generation".into(), serde_json::json!(self.generation)),
                ("pilot".into(), serde_json::json!(self.pilot.is_some())),
            ]),
        ))
    }

    fn owner(&self, client_id: &str, meta: Option<&v2::Meta>) -> agent_client_protocol::Result<()> {
        let hive = meta.and_then(|meta| meta.get("hive"));
        if self.pilot.as_deref() != Some(client_id)
            || hive
                .and_then(|hive| hive.get("generation"))
                .and_then(serde_json::Value::as_u64)
                != Some(self.generation)
            || hive
                .and_then(|hive| hive.get("token"))
                .and_then(serde_json::Value::as_str)
                != Some(&self.token)
        {
            return Err(Error::invalid_request()
                .data("stale or missing Hive controller; resume the session to take control"));
        }
        Ok(())
    }

    fn finish(&mut self, key: &str, result: Result<v2::PromptResponse, String>) {
        if let Some(record) = self.prompts.get_mut(key) {
            for waiter in record.waiters.drain(..) {
                let _ = waiter.send(result.clone());
            }
            record.result = Some(result);
        }
    }
}

fn event_meta(seq: u64) -> v2::Meta {
    let mut meta = v2::Meta::new();
    meta.insert("hive".into(), serde_json::json!({ "seq": seq }));
    meta
}

fn replay_start(
    state: &Control,
    replay: Option<&v2::ReplayFrom>,
) -> agent_client_protocol::Result<Option<u64>> {
    let first = state.events.front().map_or(state.next_seq, |(seq, _)| *seq);
    let from = match replay {
        None => None,
        Some(v2::ReplayFrom::Start(_)) => Some(1),
        Some(v2::ReplayFrom::Other(cursor)) if cursor.type_ == "_hive_seq" => Some(
            cursor
                .fields
                .get("seq")
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| {
                    Error::invalid_params().data("_hive_seq cursor requires an unsigned seq")
                })?,
        ),
        Some(_) => {
            return Err(
                Error::invalid_params().data("unsupported replay cursor; use start or _hive_seq")
            );
        }
    };
    if let Some(from) = from
        && (from < first || from > state.next_seq)
    {
        return Err(Error::invalid_request().data(format!(
            "replay cursor unavailable; available next-event range is {first}..={}",
            state.next_seq
        )));
    }
    Ok(from)
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

    fn prompt(
        &self,
        client_id: &str,
        mut request: v2::PromptRequest,
    ) -> agent_client_protocol::Result<oneshot::Receiver<Result<v2::PromptResponse, String>>> {
        let entry = self.entry(&request.session_id)?;
        let mut state = entry.control.lock().map_err(|_| Error::internal_error())?;
        state.owner(client_id, request.meta.as_ref())?;
        let key = request
            .meta
            .as_ref()
            .and_then(|m| m.get("hive"))
            .and_then(|h| h.get("key"))
            .and_then(serde_json::Value::as_str)
            .filter(|key| !key.is_empty())
            .ok_or_else(|| {
                Error::invalid_params().data("_meta.hive.key is required for idempotent prompts")
            })?
            .to_owned();
        let (tx, rx) = oneshot::channel();
        if let Some(record) = state.prompts.get_mut(&key) {
            if record.content != request.prompt {
                return Err(Error::invalid_params()
                    .data("prompt key was used with different content; choose a new key"));
            }
            if let Some(result) = &record.result {
                let _ = tx.send(result.clone());
            } else {
                record.waiters.push(tx);
            }
            return Ok(rx);
        }
        if state.running || state.closing {
            return Err(Error::invalid_request().data(
                "session has active foreground work; wait for idle or cancel before prompting",
            ));
        }
        // Reserving under the ownership lock prevents two simultaneous prompts from both starting.
        state.running = true;
        state.prompts.insert(
            key.clone(),
            PromptRecord {
                content: request.prompt.clone(),
                result: None,
                waiters: vec![tx],
            },
        );
        let generation = state.generation;
        if let Some(meta) = request.meta.as_mut() {
            meta.remove("hive");
        }
        if entry
            .commands
            .send(Command::Prompt(request, generation, key.clone()))
            .is_err()
        {
            state.prompts.remove(&key);
            state.running = false;
            return Err(Error::internal_error().data("agent disconnected; create another session"));
        }
        Ok(rx)
    }

    fn resume(
        &self,
        request: v2::ResumeSessionRequest,
        client_id: &str,
        cx: V2ConnectionTo<Client>,
    ) -> agent_client_protocol::Result<v2::ResumeSessionResponse> {
        let entry = self.entry(&request.session_id)?;
        if entry.cwd != request.cwd {
            return Err(Error::invalid_params().data("session cwd does not match"));
        }
        let mut state = entry.control.lock().map_err(|_| Error::internal_error())?;
        if state.closing {
            return Err(Error::invalid_request().data("session is closing; select another session"));
        }
        let from = replay_start(&state, request.replay_from.as_ref())?;
        let hive = request.meta.as_ref().and_then(|meta| meta.get("hive"));
        let mode = hive
            .and_then(|hive| hive.get("mode"))
            .and_then(serde_json::Value::as_str);
        if !matches!(mode, None | Some("observe") | Some("relinquish")) {
            return Err(
                Error::invalid_params().data("_meta.hive.mode must be observe or relinquish")
            );
        }
        if mode == Some("relinquish") {
            state.owner(client_id, request.meta.as_ref())?;
        }
        // Keep replay and live forwarding in one critical section: no update can cross this boundary twice.
        if let Some(from) = from {
            for (seq, update) in state.events.iter().filter(|(seq, _)| *seq >= from) {
                cx.send_notification(
                    v2::UpdateSessionNotification::new(request.session_id.clone(), update.clone())
                        .meta(event_meta(*seq)),
                )?;
            }
        }
        state.subscribers.insert(client_id.to_owned(), cx.clone());
        if mode != Some("observe") {
            state.generation = state
                .generation
                .checked_add(1)
                .ok_or_else(Error::internal_error)?;
            state.token = uuid::Uuid::new_v4().to_string();
            state.pilot = (mode != Some("relinquish")).then(|| client_id.to_owned());
            let _ = state.changed.send(state.generation);
            forward_locked(&mut state, &request.session_id, None)?;
        }
        let response = v2::ResumeSessionResponse::new().meta(state.metadata(client_id));
        let control = entry.control.clone();
        let id = request.session_id.clone();
        let subscriber = client_id.to_owned();
        tokio::spawn(async move {
            cx.incoming_closed().await;
            if let Ok(mut state) = control.lock() {
                state.subscribers.remove(&subscriber);
                if state.pilot.as_deref() == Some(&subscriber) {
                    state.pilot = None;
                    state.generation += 1;
                    state.token = uuid::Uuid::new_v4().to_string();
                    let _ = state.changed.send(state.generation);
                    let _ = forward_locked(&mut state, &id, None);
                }
            }
        });
        Ok(response)
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

    /// Close every downstream worker when an embedded gateway stops serving requests.
    pub async fn shutdown(&self) {
        let entries = self
            .sessions
            .lock()
            .map(|mut sessions| sessions.drain().map(|(_, entry)| entry).collect::<Vec<_>>())
            .unwrap_or_default();
        for entry in entries {
            let (reply, received) = oneshot::channel();
            let _ = entry.commands.send(Command::Cancel(0));
            if entry.commands.send(Command::Close(0, reply)).is_ok() {
                let _ = tokio::time::timeout(std::time::Duration::from_secs(2), received).await;
            }
            // Graceful close is best-effort; dropping the SDK connection terminates its stdio process group.
            entry.abort.abort();
        }
    }

    async fn create(
        &self,
        mut request: v2::NewSessionRequest,
        client_id: String,
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
        let control = Arc::new(Mutex::new(Control::new(client_id.clone(), gateway)));
        let cwd = request.cwd.clone();
        let entry_control = control.clone();
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
                    control,
                )
                .await;
            } else {
                run_agent(
                    provider,
                    request,
                    id.clone(),
                    rx,
                    (ready_tx, active_rx),
                    control,
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
            control: entry_control.clone(),
        };
        if let Ok(state) = entry_control.lock()
            && let Some(cx) = state.subscribers.get(&client_id).cloned()
        {
            let control = entry_control.clone();
            let subscriber = client_id.clone();
            let id = hive_id.clone();
            tokio::spawn(async move {
                cx.incoming_closed().await;
                if let Ok(mut state) = control.lock() {
                    state.subscribers.remove(&subscriber);
                    if state.pilot.as_deref() == Some(&subscriber) {
                        state.pilot = None;
                        state.generation += 1;
                        state.token = uuid::Uuid::new_v4().to_string();
                        let _ = state.changed.send(state.generation);
                        let _ = forward_locked(&mut state, &id, None);
                    }
                }
            });
        }
        match tokio::time::timeout(std::time::Duration::from_secs(15), ready_rx).await {
            Ok(Ok(Ok(mut response))) => {
                self.sessions
                    .lock()
                    .map_err(|_| Error::internal_error())?
                    .insert(hive_id.to_string(), entry);
                let _ = active_tx.send(());
                response.session_id = hive_id;
                response.meta = Some(
                    entry_control
                        .lock()
                        .map_err(|_| Error::internal_error())?
                        .metadata(&client_id),
                );
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
    control: &Mutex<Control>,
    id: &v2::SessionId,
    update: v2::SessionUpdate,
) -> agent_client_protocol::Result<()> {
    let mut state = control.lock().map_err(|_| Error::internal_error())?;
    forward_locked(&mut state, id, Some(update))
}

fn forward_locked(
    state: &mut Control,
    id: &v2::SessionId,
    update: Option<v2::SessionUpdate>,
) -> agent_client_protocol::Result<()> {
    let update = update.unwrap_or_else(|| state.status_update());
    if let v2::SessionUpdate::StateUpdate(status) = &update {
        match status {
            v2::StateUpdate::Running(_) => state.running = true,
            v2::StateUpdate::Idle(_) => state.running = false,
            _ => {}
        }
    }
    let seq = state.next_seq;
    state.next_seq += 1;
    if state.events.len() == 256 {
        state.events.pop_front();
    }
    state.events.push_back((seq, update.clone()));
    // A broken observer cannot interrupt the downstream worker or other subscribers.
    state.subscribers.retain(|_, gateway| {
        gateway
            .send_notification(
                v2::UpdateSessionNotification::new(id.clone(), update.clone())
                    .meta(event_meta(seq)),
            )
            .is_ok()
    });
    Ok(())
}

fn current(control: &Mutex<Control>, generation: u64) -> bool {
    control
        .lock()
        .is_ok_and(|state| state.generation == generation)
}

fn complete(control: &Mutex<Control>, key: &str, result: Result<v2::PromptResponse, String>) {
    if let Ok(mut state) = control.lock() {
        state.finish(key, result);
    }
}

async fn pilot_permission(
    control: Arc<Mutex<Control>>,
    request: v2::RequestPermissionRequest,
) -> v2::RequestPermissionResponse {
    let mut changes = control
        .lock()
        .expect("session state is not poisoned")
        .changed
        .subscribe();
    let answer = async {
        loop {
            let (gateway, generation) = {
                let state = control.lock().expect("session state is not poisoned");
                (
                    state
                        .pilot
                        .as_ref()
                        .and_then(|id| state.subscribers.get(id))
                        .cloned(),
                    state.generation,
                )
            };
            if let Some(gateway) = gateway {
                tokio::select! {
                    result = gateway.send_request(request.clone()).block_task() => {
                        if control.lock().expect("session state is not poisoned").generation == generation
                            && let Ok(response) = result { return response; }
                    }
                    _ = changes.changed() => continue,
                }
            } else if changes.changed().await.is_err() {
                break;
            }
            // A failed pilot request should not spin against the same connection.
            if *changes.borrow() == generation && changes.changed().await.is_err() {
                break;
            }
        }
        v2::RequestPermissionResponse::new(v2::RequestPermissionOutcome::Cancelled)
    };
    tokio::time::timeout(std::time::Duration::from_secs(120), answer)
        .await
        .unwrap_or_else(|_| {
            v2::RequestPermissionResponse::new(v2::RequestPermissionOutcome::Cancelled)
        })
}

async fn pilot_elicitation(
    control: Arc<Mutex<Control>>,
    request: v2::CreateElicitationRequest,
) -> v2::CreateElicitationResponse {
    let mut changes = control
        .lock()
        .expect("session state is not poisoned")
        .changed
        .subscribe();
    let answer = async {
        loop {
            let (gateway, generation) = {
                let state = control.lock().expect("session state is not poisoned");
                (
                    state
                        .pilot
                        .as_ref()
                        .and_then(|id| state.subscribers.get(id))
                        .cloned(),
                    state.generation,
                )
            };
            if let Some(gateway) = gateway {
                tokio::select! {
                    result = gateway.send_request(request.clone()).block_task() => {
                        if control.lock().expect("session state is not poisoned").generation == generation
                            && let Ok(response) = result { return response; }
                    }
                    _ = changes.changed() => continue,
                }
            } else if changes.changed().await.is_err() {
                break;
            }
            if *changes.borrow() == generation && changes.changed().await.is_err() {
                break;
            }
        }
        v2::CreateElicitationResponse::new(v2::ElicitationAction::Cancel)
    };
    tokio::time::timeout(std::time::Duration::from_secs(120), answer)
        .await
        .unwrap_or_else(|_| v2::CreateElicitationResponse::new(v2::ElicitationAction::Cancel))
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
    control: Arc<Mutex<Control>>,
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
                let control = control.clone();
                let hive_id = hive_id.clone();
                async move |notification: v2::UpdateSessionNotification,
                            _cx: V2ConnectionTo<Agent>| {
                    if route.lock().map_err(|_| Error::internal_error())?.as_ref()
                        != Some(&notification.session_id)
                    {
                        return Ok(());
                    }
                    forward(&control, &hive_id, notification.update)
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            {
                let control = control.clone();
                let downstream = downstream.clone();
                let hive_id = hive_id.clone();
                async move |mut request: v2::RequestPermissionRequest,
                   responder: Responder<v2::RequestPermissionResponse>,
                   _cx: V2ConnectionTo<Agent>| {
                    if downstream.lock().map_err(|_| Error::internal_error())?.as_ref() != Some(&request.session_id) {
                        return responder.respond(v2::RequestPermissionResponse::new(v2::RequestPermissionOutcome::Cancelled));
                    }
                    request.session_id = hive_id.clone();
                    responder.respond(pilot_permission(control.clone(), request).await)
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let control = control.clone();
                let downstream = downstream.clone();
                let hive_id = hive_id.clone();
                async move |mut request: v2::CreateElicitationRequest,
                    responder: Responder<v2::CreateElicitationResponse>,
                    _cx: V2ConnectionTo<Agent>| {
                    let scope = match &mut request.mode {
                        v2::ElicitationMode::Form(mode) => &mut mode.scope,
                        v2::ElicitationMode::Url(mode) => &mut mode.scope,
                        _ => return responder.respond(v2::CreateElicitationResponse::new(v2::ElicitationAction::Cancel)),
                    };
                    let v2::ElicitationScope::Session(scope) = scope else {
                        return responder.respond(v2::CreateElicitationResponse::new(v2::ElicitationAction::Cancel));
                    };
                    if downstream.lock().map_err(|_| Error::internal_error())?.as_ref() != Some(&scope.session_id) {
                        return responder.respond(v2::CreateElicitationResponse::new(v2::ElicitationAction::Cancel));
                    }
                    scope.session_id = hive_id.clone();
                    responder.respond(pilot_elicitation(control.clone(), request).await)
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(agent, async move |cx| {
            let initialized = cx
                .send_request(v2::InitializeRequest::new(
                    ProtocolVersion::V2,
                    v2::Implementation::new("hive", env!("CARGO_PKG_VERSION")),
                ).capabilities(v2::ClientCapabilities::new().elicitation(
                    v2::ElicitationCapabilities::new()
                        .form(v2::ElicitationFormCapabilities::new())
                        .url(v2::ElicitationUrlCapabilities::new()),
                )))
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
                    Command::Prompt(mut request, generation, key) => {
                        request.session_id = downstream_id.clone();
                        let prompt = {
                            let state = control.lock().expect("session state is not poisoned");
                            if state.generation != generation {
                                drop(state);
                                complete(&control, &key, Err("controller was superseded before prompt dispatch; resume with a new key".into()));
                                if let Ok(mut state) = control.lock() { state.running = false; }
                                continue;
                            }
                            // Publication under the lock defines the dispatch-before-handoff order.
                            cx.send_request(request).block_task()
                        };
                        tokio::pin!(prompt);
                        let result = loop {
                            tokio::select! {
                                result = &mut prompt => break result,
                                incoming = rx.recv() => match incoming {
                                    Some(Command::Cancel(epoch)) if epoch == 0 || current(&control, epoch) => { let _ = cx.send_notification(v2::CancelSessionNotification::new(downstream_id.clone())); }
                                    Some(Command::Cancel(_)) => {}
                                    Some(Command::Prompt(_, _, key)) => { complete(&control, &key, Err("session already has foreground work".into())); }
                                    Some(Command::Close(_, reply)) => {
                                        let _ = reply.send(Err("cancel active work before closing".into()));
                                    }
                                    None => return Ok(()),
                                }
                            }
                        }.map_err(|_| "agent prompt failed; check its availability".to_string());
                        complete(&control, &key, result);
                    }
                    Command::Cancel(epoch) => {
                        if epoch == 0 || current(&control, epoch) {
                            let _ = cx.send_notification(v2::CancelSessionNotification::new(downstream_id.clone()));
                        }
                    }
                    Command::Close(generation, reply) => {
                        if generation != 0 && !current(&control, generation) {
                            let _ = reply.send(Err("controller was superseded; resume before closing".into()));
                            continue;
                        }
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
    control: Arc<Mutex<Control>>,
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
                let control = control.clone();
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
                    match notification.update {
                        v1::SessionUpdate::AgentMessageChunk(chunk) => {
                            if let v1::ContentBlock::Text(text) = chunk.content {
                                let id = chunk_message_id.lock().map_err(|_| Error::internal_error())?.clone().ok_or_else(Error::internal_error)?;
                                forward(&control, &hive_id, v2::SessionUpdate::AgentMessageChunk(v2::ContentChunk::new(text.text.into(), id)))?;
                            }
                        }
                        v1::SessionUpdate::ToolCall(call) => {
                            let mut fields = BTreeMap::new();
                            fields.insert("tool".into(), serde_json::to_value(call).map_err(Error::into_internal_error)?);
                            forward(&control, &hive_id, v2::SessionUpdate::Other(v2::OtherSessionUpdate::new("_hive_v1_tool_call", fields)))?;
                        }
                        v1::SessionUpdate::ToolCallUpdate(call) => {
                            let mut fields = BTreeMap::new();
                            fields.insert("tool".into(), serde_json::to_value(call).map_err(Error::into_internal_error)?);
                            forward(&control, &hive_id, v2::SessionUpdate::Other(v2::OtherSessionUpdate::new("_hive_v1_tool_update", fields)))?;
                        }
                        _ => {}
                    }
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            {
                let control = control.clone();
                let route = route.clone();
                let hive_id = hive_id.clone();
                async move |request: v1::RequestPermissionRequest,
                    responder: Responder<v1::RequestPermissionResponse>,
                    _cx: ConnectionTo<Agent>| {
                    if route.lock().map_err(|_| Error::internal_error())?.as_ref() != Some(&request.session_id) {
                        return responder.respond(v1::RequestPermissionResponse::new(v1::RequestPermissionOutcome::Cancelled));
                    }
                    let options = request.options.iter().map(|option| {
                        serde_json::from_value::<v2::PermissionOption>(serde_json::to_value(option).map_err(Error::into_internal_error)?)
                            .map_err(Error::into_internal_error)
                    }).collect::<agent_client_protocol::Result<Vec<_>>>()?;
                    let title = request.tool_call.fields.title.clone().unwrap_or_else(|| "Agent tool permission".into());
                    let response = pilot_permission(control.clone(), v2::RequestPermissionRequest::new(hive_id.clone(), title, options)).await;
                    let outcome = serde_json::from_value(serde_json::to_value(response.outcome).map_err(Error::into_internal_error)?)
                        .unwrap_or(v1::RequestPermissionOutcome::Cancelled);
                    responder.respond(v1::RequestPermissionResponse::new(outcome))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let control = control.clone();
                let route = route.clone();
                let hive_id = hive_id.clone();
                async move |request: v1::CreateElicitationRequest,
                    responder: Responder<v1::CreateElicitationResponse>,
                    _cx: ConnectionTo<Agent>| {
                    let mut request: v2::CreateElicitationRequest = serde_json::from_value(
                        serde_json::to_value(request).map_err(Error::into_internal_error)?,
                    ).map_err(Error::into_internal_error)?;
                    let scope = match &mut request.mode {
                        v2::ElicitationMode::Form(mode) => &mut mode.scope,
                        v2::ElicitationMode::Url(mode) => &mut mode.scope,
                        _ => return responder.respond(v1::CreateElicitationResponse::new(v1::ElicitationAction::Cancel)),
                    };
                    let v2::ElicitationScope::Session(scope) = scope else {
                        return responder.respond(v1::CreateElicitationResponse::new(v1::ElicitationAction::Cancel));
                    };
                    if route.lock().map_err(|_| Error::internal_error())?.as_ref().map(ToString::to_string) != Some(scope.session_id.to_string()) {
                        return responder.respond(v1::CreateElicitationResponse::new(v1::ElicitationAction::Cancel));
                    }
                    scope.session_id = hive_id.clone();
                    let response = pilot_elicitation(control.clone(), request).await;
                    let response: v1::CreateElicitationResponse = serde_json::from_value(
                        serde_json::to_value(response).map_err(Error::into_internal_error)?,
                    ).map_err(Error::into_internal_error)?;
                    responder.respond(response)
                }
            },
            agent_client_protocol::on_receive_request!(),
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
                    Command::Prompt(request, generation, key) => {
                        if !current(&control, generation) {
                            complete(&control, &key, Err("controller was superseded before prompt dispatch; resume with a new key".into()));
                            if let Ok(mut state) = control.lock() { state.running = false; }
                            continue;
                        }
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
                            complete(&control, &key, Err("unsupported v2 prompt content for v1 agent".into()));
                            if let Ok(mut state) = control.lock() { state.running = false; }
                            continue;
                        };
                        if let Ok(mut current) = message_id.lock() { *current = Some(v2::MessageId::new(format!("v1-agent-{}", uuid::Uuid::new_v4()))); }
                        // Acknowledgement precedes v1 turn completion; the task then emits idle.
                        complete(&control, &key, Ok(v2::PromptResponse::new(id.clone())));
                        let user = v2::SessionUpdate::UserMessage(
                            v2::UserMessage::new(id).content(request.prompt),
                        );
                        let _ = forward(&control, &hive_id, user);
                        let _ = forward(
                            &control,
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
                                     Some(Command::Cancel(epoch)) if epoch == 0 || current(&control, epoch) => { let _ = cx.send_notification(v1::CancelNotification::new(downstream_id.clone())); }
                                     Some(Command::Cancel(_)) => {}
                                     Some(Command::Prompt(_, _, key)) => { complete(&control, &key, Err("session already has foreground work".into())); }
                                     Some(Command::Close(_, reply)) => { let _ = reply.send(Err("cancel active work before closing".into())); }
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
                            &control,
                            &hive_id,
                            v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(
                                v2::IdleStateUpdate::new().stop_reason(stop),
                            )),
                        );
                    }
                    Command::Cancel(epoch) => {
                        if epoch == 0 || current(&control, epoch) {
                            let _ = cx.send_notification(v1::CancelNotification::new(downstream_id.clone()));
                        }
                    }
                    Command::Close(generation, reply) => {
                        if generation != 0 && !current(&control, generation) {
                            let _ = reply.send(Err("controller was superseded; resume before closing".into()));
                            continue;
                        }
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
    let client_id = uuid::Uuid::new_v4().to_string();
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
                let client_id = client_id.clone();
                async move |request: v2::NewSessionRequest,
                            responder: Responder<v2::NewSessionResponse>,
                            cx: V2ConnectionTo<Client>| {
                    match broker.create(request, client_id.clone(), cx).await {
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
                let client_id = client_id.clone();
                async move |request: v2::PromptRequest,
                            responder: Responder<v2::PromptResponse>,
                            _cx: V2ConnectionTo<Client>| {
                    let received = broker.prompt(&client_id, request)?;
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
                            let state = entry.control.lock().expect("session state is not poisoned");
                            let mut meta = v2::Meta::new();
                            meta.insert("hive".into(), serde_json::json!({
                                "agent": entry.profile, "running": state.running,
                                "pilot": state.pilot.is_some(), "generation": state.generation,
                                "observers": state.subscribers.len().saturating_sub(usize::from(state.pilot.as_ref().is_some_and(|id| state.subscribers.contains_key(id)))),
                            }));
                            v2::SessionInfo::new(v2::SessionId::new(id.clone()), entry.cwd.clone()).meta(meta)
                        })
                        .collect::<Vec<_>>();
                    let mut entries = entries;
                    entries.sort_by_key(|entry| entry.session_id.to_string());
                    responder.respond(v2::ListSessionsResponse::new(entries))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let broker = broker.clone();
                let client_id = client_id.clone();
                async move |request: v2::ResumeSessionRequest,
                            responder: Responder<v2::ResumeSessionResponse>,
                            cx: V2ConnectionTo<Client>| {
                    match broker.resume(request, &client_id, cx) {
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
                let client_id = client_id.clone();
                async move |request: v2::CloseSessionRequest,
                            responder: Responder<v2::CloseSessionResponse>,
                            _cx: V2ConnectionTo<Client>| {
                    let entry = broker.entry(&request.session_id)?;
                    let (reply, received) = oneshot::channel();
                    {
                        let mut state =
                            entry.control.lock().map_err(|_| Error::internal_error())?;
                        state.owner(&client_id, request.meta.as_ref())?;
                        if state.running || state.closing {
                            return responder.respond_with_error(
                                Error::invalid_request()
                                    .data("cancel active work and wait for idle before closing"),
                            );
                        }
                        // Prevent takeover while a close has been admitted but not completed.
                        state.closing = true;
                        if entry
                            .commands
                            .send(Command::Close(state.generation, reply))
                            .is_err()
                        {
                            state.closing = false;
                            return Err(
                                Error::internal_error().data("agent disconnected during close")
                            );
                        }
                    }
                    match received.await {
                        Ok(Ok(())) => {
                            broker
                                .sessions
                                .lock()
                                .map_err(|_| Error::internal_error())?
                                .remove(&request.session_id.to_string());
                            responder.respond(v2::CloseSessionResponse::new())
                        }
                        _ => {
                            if let Ok(mut state) = entry.control.lock() {
                                state.closing = false;
                            }
                            responder.respond_with_error(
                                Error::internal_error().data("agent could not close session"),
                            )
                        }
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |request: v2::CancelSessionNotification, _cx: V2ConnectionTo<Client>| {
                let entry = broker.entry(&request.session_id)?;
                let state = entry.control.lock().map_err(|_| Error::internal_error())?;
                if state.owner(&client_id, request.meta.as_ref()).is_err() {
                    // JSON-RPC notifications have no response channel; never forward a stale cancel.
                    tracing::debug!("ignored cancel from stale Hive controller");
                    return Ok(());
                }
                entry
                    .commands
                    .send(Command::Cancel(state.generation))
                    .map_err(|_| {
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
        // LockFileEx reports ERROR_LOCK_VIOLATION (33), not WouldBlock, on Windows.
        #[cfg(windows)]
        let busy = matches!(error.raw_os_error(), Some(32 | 33));
        #[cfg(not(windows))]
        let busy = error.kind() == std::io::ErrorKind::WouldBlock;
        if busy {
            std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
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

#[cfg(test)]
mod control_tests {
    use super::*;

    fn detached() -> Control {
        let (changed, _) = watch::channel(1);
        Control {
            generation: 1,
            token: "first".into(),
            pilot: Some("test".into()),
            subscribers: HashMap::new(),
            changed,
            next_seq: 1,
            events: VecDeque::new(),
            running: false,
            closing: false,
            prompts: HashMap::new(),
        }
    }

    #[test]
    fn concurrent_handoffs_have_one_current_generation() {
        let state = Arc::new(Mutex::new(detached()));
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let workers: Vec<_> = (0..2)
            .map(|_| {
                let (state, barrier) = (state.clone(), barrier.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    let mut state = state.lock().unwrap();
                    state.generation += 1;
                    state.token = uuid::Uuid::new_v4().to_string();
                    (state.generation, state.token.clone())
                })
            })
            .collect();
        barrier.wait();
        let outcomes: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();
        let state = state.lock().unwrap();
        assert_eq!(state.generation, 3);
        for (generation, token) in outcomes {
            let mut meta = v2::Meta::new();
            meta.insert(
                "hive".into(),
                serde_json::json!({ "generation": generation, "token": token }),
            );
            assert_eq!(state.owner("test", Some(&meta)).is_ok(), generation == 3);
        }
    }

    #[test]
    fn a_replay_gap_is_visible_after_the_event_ring_wraps() {
        let state = Mutex::new(detached());
        let id = v2::SessionId::new("hive-test");
        for _ in 0..260 {
            forward(
                &state,
                &id,
                v2::SessionUpdate::StateUpdate(v2::StateUpdate::Running(
                    v2::RunningStateUpdate::new(),
                )),
            )
            .unwrap();
        }
        let state = state.lock().unwrap();
        assert_eq!(state.events.len(), 256);
        assert_eq!(state.events.front().unwrap().0, 5);
        assert_eq!(state.next_seq, 261);
        let from = v2::ReplayFrom::Other(v2::OtherReplayFrom::new(
            "_hive_seq",
            std::collections::BTreeMap::from([("seq".into(), serde_json::json!(4))]),
        ));
        assert!(replay_start(&state, Some(&from)).is_err());
        assert!(
            replay_start(
                &state,
                Some(&v2::ReplayFrom::Start(v2::ReplayFromStart::new()))
            )
            .is_err()
        );
        let available = v2::ReplayFrom::Other(v2::OtherReplayFrom::new(
            "_hive_seq",
            std::collections::BTreeMap::from([("seq".into(), serde_json::json!(5))]),
        ));
        assert_eq!(replay_start(&state, Some(&available)).unwrap(), Some(5));
        assert_eq!(
            state.generation, 1,
            "a failed cursor cannot take over control"
        );
    }
}
