//! Loopback ACPv2 gateway with a durable session catalog and live control broker.

use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    convert::Infallible,
    fs::{File, OpenOptions},
    future::IntoFuture,
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use agent_client_protocol::schema::{ProtocolVersion, v1, v2};
use agent_client_protocol::{
    AcpAgent, AcpAgentConfig, Agent, Client, ConnectTo, ConnectionTo, Error, Responder,
    V2ConnectionTo,
};
use agent_client_protocol_http::{CorsOptions, HttpClient, ServerOptions};
use axum::{
    Json, Router,
    response::{
        Html,
        sse::{Event, Sse},
    },
    routing::get,
};
use fs2::FileExt;
use hive_core::SessionId;
use hive_protocol::{AcpHttpServer, AgentVersion};
use serde::Serialize;
use tokio::sync::{mpsc, oneshot, watch};
use tokio_stream::{StreamExt, wrappers::WatchStream};

mod activity;
pub mod compat;
pub mod config;
pub mod doctor;
mod interactions;
mod opencode;
mod persistence;
mod pricing;
mod session_config;
mod usage;
use activity::Activity;
use config::{Config, Pricing, Profile, ProfileKind};
use persistence::{Outcome, Record, Store};
use usage::{Ledger, Money, Tokens, Turn};

fn storage_error(error: persistence::StoreError) -> Error {
    Error::internal_error().data(error.to_string())
}

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

struct AbortOnDrop(Option<tokio::task::AbortHandle>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            handle.abort();
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
    catalog: watch::Sender<u64>,
    store: Option<Arc<Store>>,
    offline: Arc<Mutex<HashMap<String, Record>>>,
    /// `(profile, native id)` pairs being adopted right now. Without it two simultaneous
    /// adoptions of one native session could both pass the "already attached?" check.
    adopting: Arc<Mutex<std::collections::HashSet<(String, String)>>>,
    client_capabilities: Arc<Mutex<HashMap<String, v2::ClientCapabilities>>>,
}

/// Releases an in-flight adoption claim however `create` exits.
struct AdoptGuard {
    adopting: Arc<Mutex<std::collections::HashSet<(String, String)>>>,
    key: (String, String),
}

impl Drop for AdoptGuard {
    fn drop(&mut self) {
        if let Ok(mut adopting) = self.adopting.lock() {
            adopting.remove(&self.key);
        }
    }
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
    // Only live hop mappings can identify a request-scoped question's original requester.
    parents: HashMap<v1::RequestId, interactions::Parent>,
    startup_parent: Option<interactions::Parent>,
    prompt_parents: HashMap<String, interactions::Parent>,
    client_capabilities: HashMap<String, v2::ClientCapabilities>,
    // Unlike pilot generation, this invalidates decisions permanently instead of reoffering them.
    interaction_epoch: u64,
    pending_interactions: usize,
    urls: HashMap<String, interactions::UrlRoute>,
    // Request publication and notification dispatch can race; records remain worker-local and bounded.
    early_completions: HashMap<String, (tokio::time::Instant, v2::CompleteElicitationNotification)>,
    generation: u64,
    token: String,
    pilot: Option<String>,
    subscribers: HashMap<String, V2ConnectionTo<Client>>,
    changed: watch::Sender<u64>,
    next_seq: u64,
    events: VecDeque<(u64, v2::SessionUpdate)>,
    running: bool,
    activity_known: bool,
    title: Option<String>,
    last_activity_at: Option<u64>,
    catalog: watch::Sender<u64>,
    closing: bool,
    prompts: HashMap<String, PromptRecord>,
    activity: Activity,
    recovery_required: bool,
    reattached: bool,
    store: Option<Arc<Store>>,
    record: Option<Arc<Mutex<Record>>>,
    usage: Ledger,
    active_turn: Option<String>,
    agent: String,
    pricing: Pricing,
    selected_model: Option<String>,
    config_options: Vec<v2::SessionConfigOption>,
    configuring: bool,
    /// Set while a native client (not Hive) is driving the session. Hive prompts are refused
    /// meanwhile: OpenCode cannot arbitrate between clients, so Hive must not add a second writer.
    external_activity: bool,
    /// Identity of the native session behind an `opencode-server` session.
    native: Option<NativeMeta>,
}

/// Where a Hive session lives in OpenCode, so it can be resumed there.
struct NativeMeta {
    id: String,
    /// `hive` (created by Hive) or `adopted` (found natively).
    origin: &'static str,
    /// API flavor pinned at attach time.
    api: &'static str,
}

struct PromptRecord {
    content: Vec<v2::ContentBlock>,
    result: Option<Result<v2::PromptResponse, String>>,
    waiters: Vec<oneshot::Sender<Result<v2::PromptResponse, String>>>,
}

enum Command {
    Configure(session_config::Change),
    Prompt(v2::PromptRequest, u64, String),
    Cancel(u64),
    Close(u64, oneshot::Sender<Result<(), String>>),
}

type WorkerStartup = (
    oneshot::Sender<Result<(v2::NewSessionResponse, String), String>>,
    oneshot::Receiver<()>,
);

impl Control {
    fn recovered(
        catalog: watch::Sender<u64>,
        version: AgentVersion,
        record: &Record,
        store: Arc<Store>,
        saved: Arc<Mutex<Record>>,
        pricing: Pricing,
    ) -> Self {
        let (changed, _) = watch::channel(record.generation);
        let mut activity = Activity::new();
        if version == AgentVersion::V1 {
            activity.support.insert("tasks".into(), "unsupported");
        }
        Self {
            generation: record.generation,
            parents: HashMap::new(),
            startup_parent: None,
            prompt_parents: HashMap::new(),
            client_capabilities: HashMap::new(),
            interaction_epoch: 0,
            pending_interactions: 0,
            urls: HashMap::new(),
            early_completions: HashMap::new(),
            token: uuid::Uuid::new_v4().to_string(),
            pilot: None,
            subscribers: HashMap::new(),
            changed,
            next_seq: record.next_seq,
            events: VecDeque::new(),
            running: false,
            activity_known: false,
            title: record.title.clone(),
            last_activity_at: record.last_activity_at,
            catalog,
            closing: false,
            prompts: HashMap::new(),
            activity,
            recovery_required: record.uncertain,
            reattached: true,
            store: Some(store),
            record: Some(saved),
            usage: record.usage.clone(),
            active_turn: None,
            agent: record.profile.clone(),
            pricing,
            selected_model: None,
            config_options: Vec::new(),
            configuring: false,
            external_activity: false,
            native: None,
        }
    }
    fn new(
        client_id: String,
        gateway: V2ConnectionTo<Client>,
        catalog: watch::Sender<u64>,
        version: AgentVersion,
        agent: String,
        pricing: Pricing,
    ) -> Self {
        let (changed, _) = watch::channel(1);
        let mut activity = Activity::new();
        if version == AgentVersion::V1 {
            // The stable v1 adapter cannot emit task identities.
            activity.support.insert("tasks".into(), "unsupported");
        }
        Self {
            generation: 1,
            parents: HashMap::new(),
            startup_parent: None,
            prompt_parents: HashMap::new(),
            client_capabilities: HashMap::new(),
            interaction_epoch: 0,
            pending_interactions: 0,
            urls: HashMap::new(),
            early_completions: HashMap::new(),
            token: uuid::Uuid::new_v4().to_string(),
            pilot: Some(client_id.clone()),
            subscribers: HashMap::from([(client_id, gateway)]),
            changed,
            next_seq: 1,
            events: VecDeque::new(),
            running: false,
            activity_known: false,
            title: None,
            last_activity_at: None,
            catalog,
            closing: false,
            prompts: HashMap::new(),
            activity,
            recovery_required: false,
            reattached: false,
            store: None,
            record: None,
            usage: Ledger::default(),
            active_turn: None,
            agent,
            pricing,
            selected_model: None,
            config_options: Vec::new(),
            configuring: false,
            external_activity: false,
            native: None,
        }
    }

    fn metadata(&self, client_id: &str) -> v2::Meta {
        let mut meta = v2::Meta::new();
        let mut hive = serde_json::json!({
            "generation": self.generation, "nextSeq": self.next_seq,
            "running": self.running, "pilot": self.pilot.is_some(),
            "activitySnapshot": self.activity,
            "usageSnapshot": self.usage.summary(),
            "usageSeq": self.next_seq.saturating_sub(1),
            "configSeq": self.next_seq.saturating_sub(1),
            "recoveryRequired": self.recovery_required,
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

    fn notify(&self) {
        self.catalog.send_modify(|revision| *revision += 1);
    }
}

fn now_millis() -> u64 {
    // A pre-epoch system clock cannot be represented by the unsigned wire timestamp.
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |time| time.as_millis() as u64)
}

fn record_activity(state: &mut Control) {
    // A wall-clock adjustment must not make a later observed update appear older.
    state.last_activity_at = Some(now_millis().max(state.last_activity_at.unwrap_or(0)));
}

fn prompt_title(blocks: &[v2::ContentBlock]) -> Option<String> {
    blocks.iter().find_map(|block| {
        let v2::ContentBlock::Text(text) = block else {
            return None;
        };
        let title = text.text.split_whitespace().collect::<Vec<_>>().join(" ");
        (!title.is_empty()).then(|| title.chars().take(120).collect())
    })
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSummary {
    session_id: String,
    cwd: v2::AbsolutePath,
    agent: String,
    title: Option<String>,
    lifecycle: &'static str,
    recovery: &'static str,
    recovery_reason: Option<String>,
    activity: &'static str,
    last_activity_at: Option<u64>,
    control_owner: Option<String>,
    generation: u64,
    observers: usize,
    usage: usage::Summary,
    /// `hive` when Hive created the downstream session, `adopted` when it was found natively.
    origin: String,
    /// The OpenCode session ID, for resuming the conversation in OpenCode itself.
    native_id: Option<String>,
    /// A native client is driving the session; Hive prompts are refused until it is idle.
    external_activity: bool,
}

impl SessionSummary {
    fn from_entry(id: &str, entry: &Entry, state: &Control) -> Self {
        Self {
            session_id: id.to_owned(),
            cwd: entry.cwd.clone(),
            agent: entry.profile.clone(),
            title: state.title.clone(),
            lifecycle: if state.closing { "closing" } else { "active" },
            recovery: if state.recovery_required { "recovery_required" } else if state.reattached { "reattached" } else { "active" },
            recovery_reason: state.recovery_required.then(|| "A prompt may have been interrupted; inspect agent state and explicitly acknowledge recovery before sending new work".into()),
            activity: if !state.activity_known {
                "unknown"
            } else if state.running {
                "running"
            } else {
                "idle"
            },
            last_activity_at: state.last_activity_at,
            control_owner: state.pilot.clone(),
            generation: state.generation,
            observers: state.subscribers.len().saturating_sub(usize::from(
                state
                    .pilot
                    .as_ref()
                    .is_some_and(|id| state.subscribers.contains_key(id)),
            )),
            usage: state.usage.summary(),
            origin: state
                .native
                .as_ref()
                .map_or("hive", |native| native.origin)
                .to_owned(),
            native_id: state.native.as_ref().map(|native| native.id.clone()),
            external_activity: state.external_activity,
        }
    }

    fn from_record(record: &Record) -> Self {
        Self {
            session_id: record.id.clone(),
            cwd: v2::AbsolutePath::from(PathBuf::from(&record.cwd)),
            agent: record.profile.clone(),
            title: record.title.clone(),
            lifecycle: record.outcome.as_str(),
            recovery: record.outcome.as_str(),
            recovery_reason: record.reason.clone(),
            activity: "unknown",
            last_activity_at: record.last_activity_at,
            control_owner: None,
            generation: record.generation,
            observers: 0,
            usage: record.usage.summary(),
            origin: record.origin.clone(),
            native_id: (record.kind == ProfileKind::OpencodeServer.as_str())
                .then(|| record.downstream_id.clone()),
            external_activity: false,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSnapshot {
    revision: u64,
    sessions: Vec<SessionSummary>,
    usage_by_agent: BTreeMap<String, usage::Summary>,
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

#[derive(serde::Deserialize)]
struct NativeQuery {
    directory: Option<String>,
    limit: Option<usize>,
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
    pub async fn reconcile(&self) {
        let ids = self
            .offline
            .lock()
            .map(|records| records.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        let permits = Arc::new(tokio::sync::Semaphore::new(4));
        let mut jobs = tokio::task::JoinSet::new();
        for id in ids {
            let broker = self.clone();
            let permits = permits.clone();
            jobs.spawn(async move {
                let _permit = permits.acquire().await;
                for attempt in 0..3 {
                    broker.recover(&id).await;
                    let retry = broker.offline.lock().ok().and_then(|records| records.get(&id).cloned())
                        .is_some_and(|record| matches!(record.outcome, Outcome::Unavailable)
                            && record.reason.as_deref().is_some_and(|reason| reason.contains("timed out") || reason.contains("reattachment failed") || reason.contains("session/resume failed")
                            // A server that is merely down comes back; a missing or unsupported one does not.
                            || reason.contains(opencode::error::code::UNREACHABLE) || reason.contains(opencode::error::code::STREAM_LOST)));
                    if !retry { break; }
                    if attempt == 2 {
                        if let Ok(mut offline) = broker.offline.lock() && let Some(record) = offline.get_mut(&id) {
                            record.reason = Some("agent recovery failed after three attempts; check agent state and restart Hive to retry".into());
                            if let Some(store) = &broker.store { let _ = store.save(record); }
                        }
                    } else {
                        tokio::time::sleep(std::time::Duration::from_millis(200 * (1 << attempt))).await;
                    }
                }
            });
        }
        while jobs.join_next().await.is_some() {}
    }

    async fn recover(&self, id: &str) {
        let Some(mut record) = self
            .offline
            .lock()
            .ok()
            .and_then(|records| records.get(id).cloned())
        else {
            return;
        };
        let Some(store) = self.store.clone() else {
            return;
        };
        let Some(provider) = self.registry.agents.get(&record.profile).cloned() else {
            record.outcome = Outcome::Unavailable;
            record.reason =
                Some("agent profile is missing; restore its configuration and restart Hive".into());
            self.update_offline(record);
            return;
        };
        if record.kind != provider.kind.as_str() {
            // Replaying an ACP session through the REST adapter (or the reverse) would address
            // a different conversation; refuse instead of guessing.
            record.outcome = Outcome::Unavailable;
            record.reason = Some(format!(
                "agent profile no longer uses adapter kind '{}'; restore it and restart Hive",
                record.kind
            ));
            self.update_offline(record);
            return;
        }
        let kind = provider.kind;
        let version = if record.version == "v1" {
            AgentVersion::V1
        } else {
            AgentVersion::V2
        };
        if !provider.versions.contains(&version) {
            record.outcome = Outcome::Unavailable;
            record.reason = Some("agent profile no longer enables the original ACP version; restore it and restart Hive".into());
            self.update_offline(record);
            return;
        }
        let saved = Arc::new(Mutex::new(record.clone()));
        let control = Arc::new(Mutex::new(Control::recovered(
            self.catalog.clone(),
            version,
            &record,
            store.clone(),
            saved.clone(),
            provider.pricing.clone(),
        )));
        let (tx, rx) = mpsc::unbounded_channel();
        let (ready_tx, ready_rx) = oneshot::channel();
        let (active_tx, active_rx) = oneshot::channel();
        let hive_id = v2::SessionId::new(record.id.clone());
        let cwd = v2::AbsolutePath::from(PathBuf::from(&record.cwd));
        let request = v2::NewSessionRequest::new(cwd.clone());
        let downstream_id = record.downstream_id.clone();
        let worker_control = control.clone();
        let broker = self.clone();
        let worker_id = id.to_owned();
        let worker_saved = saved.clone();
        let attach_api = record.api.as_deref().and_then(opencode::api::Flavor::parse);
        let attach_origin = if record.origin == "adopted" {
            "adopted"
        } else {
            "hive"
        };
        let worker = tokio::spawn(async move {
            if kind == ProfileKind::OpencodeServer {
                opencode::run_opencode_agent(
                    provider,
                    request,
                    hive_id,
                    rx,
                    (ready_tx, active_rx),
                    worker_control.clone(),
                    opencode::Attach::Recover {
                        id: downstream_id,
                        api: attach_api,
                        origin: attach_origin,
                    },
                )
                .await;
            } else if version == AgentVersion::V1 {
                run_v1_agent(
                    provider,
                    request,
                    hive_id,
                    rx,
                    (ready_tx, active_rx),
                    worker_control.clone(),
                    Some(downstream_id),
                )
                .await;
            } else {
                run_agent(
                    provider,
                    request,
                    hive_id,
                    rx,
                    (ready_tx, active_rx),
                    worker_control.clone(),
                    Some(downstream_id),
                )
                .await;
            }
            let closing = worker_control.lock().is_ok_and(|state| state.closing);
            if let Ok(mut state) = worker_control.lock() {
                interactions::invalidate(&mut state);
            }
            if !closing && let Ok(mut state) = worker_control.lock() {
                if let Some(record) = &state.record
                    && let Ok(mut record) = record.lock()
                {
                    record.outcome = Outcome::Unavailable;
                    record.reason =
                        Some("agent worker exited; restart Hive to retry recovery".into());
                    if let Some(store) = &state.store {
                        let _ = store.save(&record);
                    }
                }
                state.activity.session = "ended".into();
            }
            if !closing
                && let Ok(mut sessions) = broker.sessions.lock()
                && sessions.remove(&worker_id).is_some()
                && let Ok(record) = worker_saved.lock()
            {
                broker.update_offline(record.clone());
            }
        });
        let mut guard = AbortOnDrop(Some(worker.abort_handle()));
        let result = tokio::time::timeout(std::time::Duration::from_secs(12), ready_rx).await;
        match result {
            Ok(Ok(Ok((_, returned_id)))) if returned_id == record.downstream_id => {
                record.outcome = Outcome::Active;
                record.reason = None;
                if let Err(error) = store.save(&record) {
                    tracing::error!(%error, "cannot save reattached session");
                    worker.abort();
                    return;
                }
                if let Ok(mut stored) = saved.lock() {
                    *stored = record.clone();
                }
                let entry = Entry {
                    cwd,
                    profile: record.profile.clone(),
                    abort: worker.abort_handle(),
                    commands: tx,
                    control,
                };
                if let Ok(mut sessions) = self.sessions.lock() {
                    sessions.insert(id.to_owned(), entry);
                    self.offline
                        .lock()
                        .expect("offline lock poisoned")
                        .remove(id);
                    self.catalog.send_modify(|revision| *revision += 1);
                }
                if active_tx.send(()).is_err() {
                    worker.abort();
                    if let Ok(mut sessions) = self.sessions.lock() {
                        sessions.remove(id);
                    }
                    record.outcome = Outcome::Unavailable;
                    record.reason = Some(
                        "agent exited before reattachment completed; restart Hive to retry".into(),
                    );
                    self.update_offline(record);
                    return;
                }
                guard.0 = None;
                return;
            }
            Ok(Ok(Err(error))) => {
                record.outcome = if error.contains("saved session not found")
                    || error.contains(opencode::error::code::SESSION_NOT_FOUND)
                {
                    Outcome::Lost
                } else if error.contains("does not advertise")
                    || error.contains(opencode::error::code::API_UNAVAILABLE)
                    || error.contains("-32601")
                    || error.contains("-32602")
                {
                    Outcome::Orphaned
                } else {
                    Outcome::Unavailable
                };
                record.reason = Some(error);
            }
            _ => {
                record.outcome = Outcome::Unavailable;
                record.reason = Some("agent reattachment timed out or disconnected; check agent availability and restart Hive".into());
            }
        }
        worker.abort();
        self.update_offline(record);
    }

    fn update_offline(&self, record: Record) {
        if let Some(store) = &self.store
            && let Err(error) = store.save(&record)
        {
            tracing::error!(%error, "cannot record recovery failure");
        }
        if let Ok(mut offline) = self.offline.lock() {
            offline.insert(record.id.clone(), record);
            self.catalog.send_modify(|revision| *revision += 1);
        }
    }
    #[must_use]
    pub fn new(registry: Config) -> Self {
        let (catalog, _) = watch::channel(0);
        Self {
            registry: Arc::new(registry),
            sessions: Arc::new(Mutex::new(HashMap::new())),
            catalog,
            store: None,
            offline: Arc::new(Mutex::new(HashMap::new())),
            adopting: Arc::new(Mutex::new(std::collections::HashSet::new())),
            client_capabilities: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn persistent(registry: Config, path: PathBuf) -> Result<Self, persistence::StoreError> {
        let store = Arc::new(Store::open(path)?);
        // Retain interrupted keys indefinitely; only definitely non-resumable old records age out.
        store.prune(now_millis().saturating_sub(90 * 24 * 60 * 60 * 1000))?;
        let mut broker = Self::new(registry);
        broker.offline = Arc::new(Mutex::new(
            store
                .records()?
                .into_iter()
                .map(|mut r| {
                    r.generation = r.generation.saturating_add(1);
                    r.next_seq = r.reserved_seq.saturating_add(1).max(r.next_seq);
                    r.outcome = Outcome::Recovering;
                    (r.id.clone(), r)
                })
                .collect(),
        ));
        broker.store = Some(store);
        Ok(broker)
    }

    fn snapshot(
        &self,
        cwd: Option<&v2::AbsolutePath>,
    ) -> agent_client_protocol::Result<SessionSnapshot> {
        loop {
            let before = *self.catalog.borrow();
            let sessions = self.sessions.lock().map_err(|_| Error::internal_error())?;
            let mut summaries = sessions
                .iter()
                .filter(|(_, entry)| cwd.is_none_or(|cwd| cwd == &entry.cwd))
                .map(|(id, entry)| {
                    let state = entry.control.lock().map_err(|_| Error::internal_error())?;
                    Ok(SessionSummary::from_entry(id, entry, &state))
                })
                .collect::<agent_client_protocol::Result<Vec<_>>>()?;
            let offline = self.offline.lock().map_err(|_| Error::internal_error())?;
            summaries.extend(
                offline
                    .values()
                    .filter(|record| {
                        cwd.is_none_or(|cwd| cwd.0 == std::path::Path::new(&record.cwd))
                    })
                    .map(SessionSummary::from_record),
            );
            // An owner/activity mutation can interleave between individual control locks.
            // Retrying ensures the snapshot never claims a revision for mixed states.
            let after = *self.catalog.borrow();
            if before == after {
                summaries.sort_by(|a, b| a.session_id.cmp(&b.session_id));
                let mut usage_by_agent: BTreeMap<String, usage::Summary> = BTreeMap::new();
                for summary in &summaries {
                    match usage_by_agent.entry(summary.agent.clone()) {
                        std::collections::btree_map::Entry::Vacant(entry) => {
                            entry.insert(summary.usage.clone());
                        }
                        std::collections::btree_map::Entry::Occupied(mut entry) => {
                            entry.get_mut().combine(&summary.usage);
                        }
                    }
                }
                return Ok(SessionSnapshot {
                    revision: after,
                    sessions: summaries,
                    usage_by_agent,
                });
            }
        }
    }

    fn entry(&self, id: &v2::SessionId) -> agent_client_protocol::Result<Entry> {
        self.sessions
            .lock()
            .map_err(|_| Error::internal_error())?
            .get(&id.to_string())
            .cloned()
            .ok_or_else(|| {
                if let Ok(offline) = self.offline.lock() && let Some(record) = offline.get(&id.to_string()) {
                    return Error::invalid_request().data(format!("Hive session {id} is {}: {}; inspect the catalog and retry recovery or choose another session", record.outcome.as_str(), record.reason.as_deref().unwrap_or("agent reattachment pending")));
                }
                Error::invalid_params().data(format!("unknown Hive session {id}"))
            })
    }

    /// The Hive session already bound to a native OpenCode session, if any.
    ///
    /// One native session maps to exactly one Hive session: this is what keeps adoption from
    /// creating a duplicate conversation, so it covers both live and offline (recovering) sessions.
    fn find_native(&self, profile: &str, native_id: &str) -> Option<String> {
        if let Ok(sessions) = self.sessions.lock() {
            for (id, entry) in sessions.iter() {
                if entry.profile == profile
                    && entry
                        .control
                        .lock()
                        .is_ok_and(|state| state.native.as_ref().is_some_and(|n| n.id == native_id))
                {
                    return Some(id.clone());
                }
            }
        }
        let offline = self.offline.lock().ok()?;
        offline
            .values()
            .find(|record| {
                record.profile == profile
                    && record.kind == ProfileKind::OpencodeServer.as_str()
                    && record.downstream_id == native_id
            })
            .map(|record| record.id.clone())
    }

    /// Claims a native session for adoption, or explains who already holds it.
    fn begin_adopt(
        &self,
        profile: &str,
        native_id: &str,
    ) -> Result<AdoptGuard, opencode::error::OpencodeError> {
        let key = (profile.to_owned(), native_id.to_owned());
        // Claim first, then look: a concurrent adopter now sees our claim, and we see its
        // finished session.
        let claimed = self
            .adopting
            .lock()
            .is_ok_and(|mut adopting| adopting.insert(key.clone()));
        if !claimed {
            return Err(opencode::error::OpencodeError::AdoptionInProgress {
                native_id: native_id.to_owned(),
            });
        }
        // Built only after a successful claim, so dropping it never releases someone else's.
        let guard = AdoptGuard {
            adopting: self.adopting.clone(),
            key,
        };
        match self.find_native(profile, native_id) {
            Some(hive_id) => Err(opencode::error::OpencodeError::AlreadyAdopted {
                native_id: native_id.to_owned(),
                hive_id,
            }),
            None => Ok(guard),
        }
    }

    fn prompt(
        &self,
        client_id: &str,
        mut request: v2::PromptRequest,
        mut parent: interactions::Parent,
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
            // A retried logical prompt has multiple RPC waiters, hence no unique parent identity.
            state.prompt_parents.remove(&key);
            state.parents.retain(|_, parent| {
                if parent.prompt_key.as_deref() == Some(&key) {
                    parent.invalidate();
                    false
                } else {
                    true
                }
            });
            return Ok(rx);
        }
        if let Some(store) = &state.store
            && let Some((content, result)) = store
                .prompt(&request.session_id.to_string(), &key)
                .map_err(storage_error)?
        {
            if content
                != serde_json::to_string(&request.prompt).map_err(Error::into_internal_error)?
            {
                return Err(Error::invalid_params()
                    .data("prompt key was used with different content; choose a new key"));
            }
            if let Some(result) = result {
                let outcome: Result<v2::PromptResponse, String> =
                    serde_json::from_str(&result).map_err(Error::into_internal_error)?;
                let _ = tx.send(outcome);
                return Ok(rx);
            }
            return Err(Error::invalid_request().data("prompt outcome is uncertain after restart; never retry this key; inspect agent state and explicitly acknowledge recovery"));
        }
        if state.recovery_required {
            return Err(Error::invalid_request().data("session recovery requires explicit acknowledgement on session/resume before new prompts"));
        }
        if state.external_activity {
            // Say *why* it is busy: the generic message would send users hunting for a Hive prompt.
            return Err(opencode::error::OpencodeError::ExternallyControlled.to_acp());
        }
        if state.running || state.closing || state.configuring {
            return Err(Error::invalid_request().data(
                "session has active foreground work; wait for idle or cancel before prompting",
            ));
        }
        // Reserving under the ownership lock prevents two simultaneous prompts from both starting.
        let previous_activity = state.activity_known;
        let previous_time = state.last_activity_at;
        state.running = true;
        state.activity_known = true;
        record_activity(&mut state);
        let title = state
            .title
            .clone()
            .or_else(|| prompt_title(&request.prompt));
        state.prompts.insert(
            key.clone(),
            PromptRecord {
                content: request.prompt.clone(),
                result: None,
                waiters: vec![tx],
            },
        );
        state.active_turn = Some(key.clone());
        if let (Some(store), Some(record)) = (state.store.clone(), state.record.clone()) {
            let mut record = record.lock().map_err(|_| Error::internal_error())?;
            record.uncertain = true;
            record.last_activity_at = state.last_activity_at;
            record.title = title.clone();
            let content =
                serde_json::to_string(&request.prompt).map_err(Error::into_internal_error)?;
            if let Err(error) = store.reserve_prompt(&record, &key, &content) {
                state.prompts.remove(&key);
                state.active_turn = None;
                state.running = false;
                state.recovery_required = true;
                return Err(storage_error(error));
            }
        }
        let generation = state.generation;
        parent.prompt_key = Some(key.clone());
        state.prompt_parents.insert(key.clone(), parent);
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
            state.recovery_required = state.store.is_some();
            state.activity_known = previous_activity;
            state.last_activity_at = previous_time;
            return Err(Error::internal_error().data("agent disconnected; create another session"));
        }
        state.title = title;
        state.notify();
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
        if let Some(capabilities) = self
            .client_capabilities
            .lock()
            .map_err(|_| Error::internal_error())?
            .get(client_id)
        {
            state
                .client_capabilities
                .insert(client_id.to_owned(), capabilities.clone());
        }
        let action = request
            .meta
            .as_ref()
            .and_then(|meta| meta.get("hive"))
            .and_then(|hive| hive.get("recoveryAction"))
            .and_then(serde_json::Value::as_str);
        if !matches!(action, None | Some("acknowledge")) {
            return Err(
                Error::invalid_params().data("_meta.hive.recoveryAction must be acknowledge")
            );
        }
        if action.is_some() && !state.recovery_required {
            return Err(
                Error::invalid_request().data("session does not require recovery acknowledgement")
            );
        }
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
        if action.is_some() && mode.is_some() {
            return Err(Error::invalid_params()
                .data("recovery acknowledgement requires pilot takeover; omit _meta.hive.mode"));
        }
        if mode == Some("relinquish") {
            state.owner(client_id, request.meta.as_ref())?;
        }
        if state.recovery_required && mode != Some("observe") && action != Some("acknowledge") {
            return Err(Error::invalid_request().data("interrupted generation may have produced external effects; inspect agent state and explicitly acknowledge with _meta.hive.recoveryAction = acknowledge on session/resume"));
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
        if mode != Some("observe") {
            let next_generation = state
                .generation
                .checked_add(1)
                .ok_or_else(Error::internal_error)?;
            if let (Some(store), Some(record)) = (&state.store, &state.record) {
                let mut record = record.lock().map_err(|_| Error::internal_error())?;
                let mut next = record.clone();
                next.generation = next_generation;
                if action.is_some() {
                    next.uncertain = false;
                }
                store.save(&next).map_err(storage_error)?;
                *record = next;
            }
            state.generation = next_generation;
            if action.is_some() {
                state.recovery_required = false;
            }
        }
        let attached = state
            .subscribers
            .insert(client_id.to_owned(), cx.clone())
            .is_none();
        if mode != Some("observe") {
            state.token = uuid::Uuid::new_v4().to_string();
            state.pilot = (mode != Some("relinquish")).then(|| client_id.to_owned());
            let _ = state.changed.send(state.generation);
            state.notify();
            forward_locked(&mut state, &request.session_id, None)?;
        } else if attached {
            state.notify();
        }
        let response = v2::ResumeSessionResponse::new()
            .config_options(state.config_options.clone())
            .meta(state.metadata(client_id));
        let control = entry.control.clone();
        let id = request.session_id.clone();
        let subscriber = client_id.to_owned();
        tokio::spawn(async move {
            cx.incoming_closed().await;
            if let Ok(mut state) = control.lock() {
                let removed = state.subscribers.remove(&subscriber).is_some();
                state.client_capabilities.remove(&subscriber);
                if state.pilot.as_deref() == Some(&subscriber) {
                    state.pilot = None;
                    state.generation += 1;
                    state.token = uuid::Uuid::new_v4().to_string();
                    let _ = state.changed.send(state.generation);
                    state.notify();
                    let _ = forward_locked(&mut state, &id, None);
                } else if removed {
                    state.notify();
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
        let offline = self
            .offline
            .lock()
            .map(|offline| offline.len())
            .unwrap_or_default();
        Status {
            name: "hive",
            version: env!("CARGO_PKG_VERSION"),
            sessions: sessions.map_or(0, |sessions| sessions.len()) + offline,
            default_agent: self.registry.default_agent.clone(),
            active_by_agent,
        }
    }

    /// Native sessions an `opencode-server` profile can adopt, marked with their Hive ID if any.
    async fn native_sessions(
        &self,
        agent: &str,
        query: NativeQuery,
    ) -> Result<serde_json::Value, (axum::http::StatusCode, Json<serde_json::Value>)> {
        use axum::http::StatusCode;
        let problem = |status, code: &str, message: String| {
            (
                status,
                Json(serde_json::json!({ "code": code, "message": message })),
            )
        };
        let profile = self.registry.agents.get(agent).ok_or_else(|| {
            problem(
                StatusCode::NOT_FOUND,
                "hive::providers::unknown",
                format!("unknown agent profile '{agent}'; query GET /v1/providers"),
            )
        })?;
        if profile.kind != ProfileKind::OpencodeServer {
            return Err(problem(
                StatusCode::BAD_REQUEST,
                "hive::providers::not_adoptable",
                format!(
                    "agent '{agent}' has no native sessions; only kind = 'opencode-server' profiles do"
                ),
            ));
        }
        let discovery = opencode::discover(
            profile,
            query
                .directory
                .as_deref()
                .filter(|directory| !directory.is_empty()),
            query.limit.unwrap_or(50).clamp(1, 200),
        )
        .await
        .map_err(|error| {
            let data = serde_json::json!({
                "code": error.code(), "message": error.to_string(), "help": error.help(),
            });
            // 502: the failure is upstream (the OpenCode server), not in the caller's request.
            (StatusCode::BAD_GATEWAY, Json(data))
        })?;
        let sessions = discovery
            .sessions
            .into_iter()
            .map(|found| {
                serde_json::json!({
                    "nativeId": found.session.id,
                    "title": found.session.title,
                    "directory": found.session.directory,
                    "updatedAt": found.session.updated,
                    "busy": found.busy,
                    "adoptedAs": self.find_native(agent, &found.session.id),
                })
            })
            .collect::<Vec<_>>();
        Ok(serde_json::json!({
            "agent": agent,
            "server": { "version": discovery.version, "api": discovery.api },
            "sessions": sessions,
        }))
    }

    fn provider_info(&self) -> serde_json::Value {
        serde_json::json!({ "defaultAgent": self.registry.default_agent, "agents": self.registry.public_profiles() })
    }

    /// Close every downstream worker when an embedded gateway stops serving requests.
    pub async fn shutdown(&self) {
        let entries = self
            .sessions
            .lock()
            .map(|mut sessions| {
                let entries = sessions.drain().map(|(_, entry)| entry).collect::<Vec<_>>();
                if !entries.is_empty() {
                    self.catalog.send_modify(|revision| *revision += 1);
                }
                entries
            })
            .unwrap_or_default();
        for entry in entries {
            // Shutdown is not an explicit user close: preserve downstream state for reattachment.
            entry.abort.abort();
        }
    }

    async fn create(
        &self,
        mut request: v2::NewSessionRequest,
        client_id: String,
        gateway: V2ConnectionTo<Client>,
        parent: interactions::Parent,
    ) -> agent_client_protocol::Result<v2::NewSessionResponse> {
        let requested = request.meta.as_mut().and_then(|meta| meta.get_mut("hive"));
        let mut adopt: Option<String> = None;
        let chosen = match requested {
            Some(serde_json::Value::Object(hive)) => {
                if let Some(value) = hive.remove("adopt") {
                    adopt = Some(
                        value
                            .get("nativeId")
                            .and_then(serde_json::Value::as_str)
                            .filter(|id| !id.is_empty())
                            .map(str::to_owned)
                            .ok_or_else(|| Error::invalid_params().data(
                                "_meta.hive.adopt must be an object with a nonempty nativeId; list candidates with GET /v1/providers/<agent>/native-sessions",
                            ))?,
                    );
                }
                let id =
                    match hive.remove("agent") {
                        Some(serde_json::Value::String(id)) if !id.is_empty() => id,
                        _ => return Err(Error::invalid_params().data(
                            "_meta.hive.agent must be a nonempty profile ID; see GET /v1/providers",
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
                "unknown agent profile '{chosen}'; query GET /v1/providers"
            ))
        })?;
        let kind = provider.kind;
        if adopt.is_some() && kind != ProfileKind::OpencodeServer {
            return Err(Error::invalid_params().data(format!(
                "agent '{chosen}' cannot adopt native sessions; _meta.hive.adopt needs a profile with kind = 'opencode-server'"
            )));
        }
        // Held until this function returns: the session is registered (and findable) or failed.
        let _adoption = match &adopt {
            Some(native_id) => Some(
                self.begin_adopt(&chosen, native_id)
                    .map_err(|error| error.to_acp())?,
            ),
            None => None,
        };
        let hive_id = v2::SessionId::new(SessionId::new().as_str());
        let (tx, rx) = mpsc::unbounded_channel();
        let (ready_tx, ready_rx) = oneshot::channel();
        let (active_tx, active_rx) = oneshot::channel();
        let control = Arc::new(Mutex::new(Control::new(
            client_id.clone(),
            gateway,
            self.catalog.clone(),
            provider.preferred_version(),
            chosen.clone(),
            provider.pricing.clone(),
        )));
        {
            let mut state = control.lock().map_err(|_| Error::internal_error())?;
            state
                .client_capabilities
                .insert(client_id.clone(), parent.capabilities.clone());
            state.startup_parent = Some(parent);
        }
        let cwd = request.cwd.clone();
        let entry_control = control.clone();
        let worker_control = control.clone();
        let id = hive_id.clone();
        let sessions = self.sessions.clone();
        let offline = self.offline.clone();
        let catalog = self.catalog.clone();
        let store = self.store.clone();
        let stored_record = self.store.as_ref().map(|_| {
            Arc::new(Mutex::new(Record {
                id: hive_id.to_string(),
                downstream_id: String::new(),
                cwd: cwd.0.display().to_string(),
                profile: chosen.clone(),
                version: if provider.preferred_version() == AgentVersion::V1 {
                    "v1"
                } else {
                    "v2"
                }
                .into(),
                created_at: now_millis(),
                updated_at: now_millis(),
                last_activity_at: None,
                title: None,
                next_seq: 1,
                reserved_seq: 0,
                generation: 1,
                outcome: Outcome::Active,
                reason: None,
                uncertain: false,
                usage: Ledger::default(),
                kind: kind.as_str().into(),
                api: None,
                origin: if adopt.is_some() { "adopted" } else { "hive" }.into(),
            }))
        });
        let worker_record = stored_record.clone();
        // Each session owns its subprocess: no guessed transport support or duplicate probing.
        let task = tokio::spawn(async move {
            if kind == ProfileKind::OpencodeServer {
                opencode::run_opencode_agent(
                    provider,
                    request,
                    id.clone(),
                    rx,
                    (ready_tx, active_rx),
                    control,
                    match adopt {
                        Some(native_id) => opencode::Attach::Adopt(native_id),
                        None => opencode::Attach::New,
                    },
                )
                .await;
            } else if provider.preferred_version() == AgentVersion::V1 {
                run_v1_agent(
                    provider,
                    request,
                    id.clone(),
                    rx,
                    (ready_tx, active_rx),
                    control,
                    None,
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
                    None,
                )
                .await;
            }
            if let Ok(mut state) = worker_control.lock() {
                interactions::invalidate(&mut state);
                state.activity.session = "ended".into();
                let _ = publish_activity(
                    &mut state,
                    &id,
                    serde_json::json!({
                        "kind": "session", "status": "ended", "source": "hive",
                    }),
                );
            }
            let closing = worker_control.lock().is_ok_and(|state| state.closing);
            if !closing
                && let (Some(store), Some(record)) = (&store, &worker_record)
                && let Ok(mut record) = record.lock()
            {
                if record.downstream_id.is_empty() {
                    return;
                }
                record.outcome = Outcome::Unavailable;
                record.reason =
                    Some("downstream worker exited; restart Hive to attempt reattachment".into());
                if let Err(error) = store.save(&record) {
                    tracing::error!(%error, "cannot persist worker exit");
                }
                if let Ok(mut offline) = offline.lock() {
                    offline.insert(id.to_string(), record.clone());
                }
            }
            // A crashed or closed subprocess must not remain advertised as active.
            if let Ok(mut sessions) = sessions.lock()
                && sessions.remove(&id.to_string()).is_some()
            {
                catalog.send_modify(|revision| *revision += 1);
            }
        });
        let entry = Entry {
            cwd,
            profile: chosen,
            abort: task.abort_handle(),
            commands: tx,
            control: entry_control.clone(),
        };
        // Dropping a not-yet-registered creation must not strand an invisible downstream worker.
        let mut startup_guard = AbortOnDrop(Some(task.abort_handle()));
        if let Ok(state) = entry_control.lock()
            && let Some(cx) = state.subscribers.get(&client_id).cloned()
        {
            let control = entry_control.clone();
            let subscriber = client_id.clone();
            let id = hive_id.clone();
            tokio::spawn(async move {
                cx.incoming_closed().await;
                if let Ok(mut state) = control.lock() {
                    let removed = state.subscribers.remove(&subscriber).is_some();
                    state.client_capabilities.remove(&subscriber);
                    if state.pilot.as_deref() == Some(&subscriber) {
                        state.pilot = None;
                        state.generation += 1;
                        state.token = uuid::Uuid::new_v4().to_string();
                        let _ = state.changed.send(state.generation);
                        state.notify();
                        let _ = forward_locked(&mut state, &id, None);
                    } else if removed {
                        state.notify();
                    }
                }
            });
        }
        match interactions::startup(&entry_control, ready_rx).await {
            Ok(Ok(Ok((mut response, downstream_id)))) => {
                if let (Some(store), Some(record)) = (&self.store, &stored_record) {
                    let mut record = record.lock().map_err(|_| Error::internal_error())?;
                    record.downstream_id = downstream_id;
                    {
                        let state = entry_control.lock().map_err(|_| Error::internal_error())?;
                        // Pin what the worker decided at attach time, so recovery never swaps it.
                        if let Some(native) = &state.native {
                            record.api = Some(native.api.to_owned());
                        }
                        if state.title.is_some() {
                            record.title = state.title.clone();
                        }
                        record.last_activity_at = state.last_activity_at;
                    }
                    record.next_seq = entry_control
                        .lock()
                        .map_err(|_| Error::internal_error())?
                        .next_seq;
                    record.reserved_seq = record.next_seq.saturating_sub(1);
                    if let Err(error) = store.save(&record) {
                        task.abort();
                        return Err(storage_error(error));
                    }
                    let mut state = entry_control.lock().map_err(|_| Error::internal_error())?;
                    state.store = Some(store.clone());
                    state.record = stored_record.clone();
                }
                {
                    let mut sessions = self.sessions.lock().map_err(|_| Error::internal_error())?;
                    sessions.insert(hive_id.to_string(), entry);
                    self.catalog.send_modify(|revision| *revision += 1);
                }
                let _ = active_tx.send(());
                startup_guard.0 = None;
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
                Err(startup_error(message))
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

/// Startup failures arrive as text. Diagnostics from the OpenCode adapter lead with their
/// `hive::opencode::<kind>` code; surface that as a field so clients need not parse prose.
fn startup_error(message: String) -> Error {
    match message.split_once(": ") {
        Some((code, rest)) if code.starts_with("hive::opencode::") => {
            Error::internal_error().data(serde_json::json!({ "code": code, "message": rest }))
        }
        _ => Error::internal_error().data(message),
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

fn forward_v1(
    control: &Mutex<Control>,
    id: &v2::SessionId,
    source: &v1::SessionUpdate,
    raw: v2::SessionUpdate,
) -> agent_client_protocol::Result<()> {
    let mut state = control.lock().map_err(|_| Error::internal_error())?;
    forward_locked(&mut state, id, Some(raw))?;
    if let Some(event) = state.activity.v1(source) {
        publish_activity(&mut state, id, event)?;
    }
    Ok(())
}

fn publish_usage(
    state: &mut Control,
    id: &v2::SessionId,
    turn: Option<&Turn>,
) -> agent_client_protocol::Result<()> {
    let value = serde_json::json!({
        "version": 1, "scope": "turn", "source": "acp",
        "turn": turn, "session": state.usage.summary(),
    });
    let serde_json::Value::Object(fields) = value else {
        unreachable!()
    };
    forward_locked(
        state,
        id,
        Some(v2::SessionUpdate::Other(v2::OtherSessionUpdate::new(
            "_hive_usage",
            fields.into_iter().collect(),
        ))),
    )
}

fn save_usage(state: &mut Control) -> agent_client_protocol::Result<()> {
    if let (Some(store), Some(record)) = (&state.store, &state.record) {
        let mut record = record.lock().map_err(|_| Error::internal_error())?;
        record.usage = state.usage.clone();
        store.save(&record).map_err(storage_error)?;
    }
    Ok(())
}

fn report_v1_cost(
    control: &Mutex<Control>,
    id: &v2::SessionId,
    update: &v1::UsageUpdate,
) -> agent_client_protocol::Result<()> {
    let mut usage = v2::UsageUpdate::new(update.used, update.size);
    if let Some(cost) = update
        .cost
        .as_ref()
        .and_then(|cost| Money::valid(cost.amount, &cost.currency))
    {
        usage = usage.cost(v2::Cost::new(cost.amount, cost.currency));
    }
    forward(control, id, v2::SessionUpdate::UsageUpdate(usage))
}

fn publish_activity(
    state: &mut Control,
    id: &v2::SessionId,
    event: serde_json::Value,
) -> agent_client_protocol::Result<()> {
    let serde_json::Value::Object(mut fields) = event else {
        return Ok(());
    };
    fields.insert("version".into(), serde_json::json!(1));
    state.activity.seq = state.next_seq;
    forward_locked(
        state,
        id,
        Some(v2::SessionUpdate::Other(v2::OtherSessionUpdate::new(
            "_hive_activity",
            fields.into_iter().collect(),
        ))),
    )
}

fn forward_locked(
    state: &mut Control,
    id: &v2::SessionId,
    update: Option<v2::SessionUpdate>,
) -> agent_client_protocol::Result<()> {
    let is_agent_update = update.is_some();
    let update = update.unwrap_or_else(|| state.status_update());
    if let v2::SessionUpdate::UsageUpdate(usage) = &update {
        let cost = usage
            .cost
            .as_ref()
            .and_then(|cost| Money::valid(cost.amount, &cost.currency));
        state.usage.report_cost(usage.used, usage.size, cost);
        save_usage(state)?;
    }
    if let v2::SessionUpdate::ConfigOptionUpdate(options) = &update {
        state.selected_model = selected_model(&options.config_options);
        state.config_options = options.config_options.clone();
    }
    if is_agent_update {
        record_activity(state);
    }
    // Commit accounting before publishing idle: a crash cannot announce a completed
    // turn while forgetting the corresponding durable tokens on restart.
    let finished_turn = if let v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(idle)) = &update
        && let Some(key) = state.active_turn.take()
    {
        let turn = Turn {
            id: key,
            agent: state.agent.clone(),
            timestamp: now_millis(),
            tokens: idle.usage.as_deref().map(Tokens::from),
            estimate: None,
        };
        if state.usage.record(turn.clone()) {
            save_usage(state)?;
            Some(turn)
        } else {
            None
        }
    } else {
        None
    };
    if let v2::SessionUpdate::StateUpdate(status) = &update {
        match status {
            v2::StateUpdate::Running(_) => {
                state.running = true;
                state.activity_known = true;
            }
            v2::StateUpdate::Idle(_) => {
                state.running = false;
                state.activity_known = true;
                if let (Some(store), Some(record)) = (&state.store, &state.record)
                    && !state.recovery_required
                    && state.prompts.values().all(|prompt| prompt.result.is_some())
                {
                    let mut record = record.lock().map_err(|_| Error::internal_error())?;
                    record.uncertain = false;
                    store.save(&record).map_err(storage_error)?;
                }
            }
            _ => {}
        }
    }
    let seq = state.next_seq;
    if let (Some(store), Some(record)) = (&state.store, &state.record) {
        let mut record = record.lock().map_err(|_| Error::internal_error())?;
        if seq > record.reserved_seq {
            // Reserve a whole block durably before publishing any cursor in it; after a crash
            // skipped numbers make the loss visible rather than reusing old client cursors.
            record.reserved_seq = seq.saturating_add(1023);
            store.save(&record).map_err(storage_error)?;
        }
    }
    state.next_seq += 1;
    if state.events.len() == 256 {
        state.events.pop_front();
    }
    state.events.push_back((seq, update.clone()));
    // A broken observer cannot interrupt the downstream worker or other subscribers.
    let subscribers = state.subscribers.len();
    state.subscribers.retain(|_, gateway| {
        gateway
            .send_notification(
                v2::UpdateSessionNotification::new(id.clone(), update.clone())
                    .meta(event_meta(seq)),
            )
            .is_ok()
    });
    if is_agent_update || subscribers != state.subscribers.len() {
        state.notify();
    }
    if let Some(event) = state.activity.v2(&update) {
        publish_activity(state, id, event)?;
    }
    if let Some(turn) = finished_turn {
        publish_usage(state, id, Some(&turn))?;
    } else if matches!(update, v2::SessionUpdate::UsageUpdate(_)) {
        publish_usage(state, id, None)?;
    }
    Ok(())
}

fn selected_model(options: &[v2::SessionConfigOption]) -> Option<String> {
    let models = options
        .iter()
        .filter_map(|option| {
            let value = serde_json::to_value(option).ok()?;
            (value["category"] == "model")
                .then(|| value["currentValue"].as_str().map(str::to_owned))
                .flatten()
        })
        .collect::<Vec<_>>();
    (models.len() == 1)
        .then(|| models.into_iter().next())
        .flatten()
}

fn schedule_price(control: Arc<Mutex<Control>>, id: v2::SessionId) {
    let target = control.lock().ok().and_then(|state| {
        let turn = state.usage.turns.last()?;
        if turn.estimate.is_some() || turn.tokens.is_none() {
            return None;
        }
        let provider = state.pricing.provider.clone()?;
        let model = if state.pricing.models.is_empty() {
            state.pricing.model.clone()?
        } else {
            state
                .pricing
                .models
                .get(state.selected_model.as_ref()?)?
                .clone()
        };
        Some((
            turn.id.clone(),
            provider,
            model,
            state.pricing.cached_input_included,
        ))
    });
    let Some((key, provider, model, cached_input_included)) = target else {
        return;
    };
    tokio::spawn(async move {
        let Some(rate) = pricing::lookup(&provider, &model).await else {
            return;
        };
        if let Ok(mut state) = control.lock() {
            let Some(turn) = state
                .usage
                .turns
                .iter_mut()
                .find(|turn| turn.id == key && turn.estimate.is_none())
            else {
                return;
            };
            let Some(tokens) = &turn.tokens else {
                return;
            };
            turn.estimate = Some(rate.estimate(&provider, &model, tokens, cached_input_included));
            let turn = turn.clone();
            if let Err(error) =
                save_usage(&mut state).and_then(|()| publish_usage(&mut state, &id, Some(&turn)))
            {
                tracing::warn!(%error, "could not publish usage price");
            }
        }
    });
}

fn current(control: &Mutex<Control>, generation: u64) -> bool {
    control
        .lock()
        .is_ok_and(|state| state.generation == generation)
}

fn complete(control: &Mutex<Control>, key: &str, result: Result<v2::PromptResponse, String>) {
    if let Ok(mut state) = control.lock() {
        if let (Some(store), Some(record)) = (state.store.clone(), state.record.clone())
            && let Ok(record) = record.lock()
        {
            let persisted = serde_json::to_string(&result)
                .map_err(|error| error.to_string())
                .and_then(|encoded| {
                    store
                        .finish_prompt(&record.id, key, &encoded)
                        .map_err(|error| error.to_string())
                });
            if let Err(error) = persisted {
                tracing::error!(%error, "cannot persist prompt result");
                state.recovery_required = true;
                state.finish(key, Err("cannot persist prompt outcome; inspect agent state and acknowledge recovery before new work".into()));
                state.notify();
                return;
            }
        }
        state.finish(key, result);
    }
}

async fn pilot_permission(
    control: Arc<Mutex<Control>>,
    request: v2::RequestPermissionRequest,
) -> v2::RequestPermissionResponse {
    interactions::permission(control, request).await
}

async fn pilot_elicitation(
    control: Arc<Mutex<Control>>,
    request: v2::CreateElicitationRequest,
) -> v2::CreateElicitationResponse {
    interactions::elicitation(control, request).await
}

async fn run_agent(
    provider: Profile,
    request: v2::NewSessionRequest,
    hive_id: v2::SessionId,
    mut rx: mpsc::UnboundedReceiver<Command>,
    startup: WorkerStartup,
    control: Arc<Mutex<Control>>,
    recovering: Option<String>,
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
                    let idle = matches!(notification.update, v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(_)));
                    forward(&control, &hive_id, notification.update)?;
                    if idle { schedule_price(control.clone(), hive_id.clone()); }
                    Ok(())
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
                   cx: V2ConnectionTo<Agent>| {
                    if downstream.lock().map_err(|_| Error::internal_error())?.as_ref() != Some(&request.session_id) {
                        return responder.respond(v2::RequestPermissionResponse::new(v2::RequestPermissionOutcome::Cancelled));
                    }
                    request.session_id = hive_id.clone();
                    let control = control.clone();
                    let epoch = control.lock().map_err(|_| Error::internal_error())?.interaction_epoch;
                    let cancellation = responder.cancellation();
                    cx.spawn(async move {
                        let result = cancellation.run_until_cancelled(async { Ok(interactions::permission_at(control, request, epoch).await) }).await;
                        let _ = responder.respond_with_result(result);
                        Ok(())
                    })
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
                    cx: V2ConnectionTo<Agent>| {
                    let epoch = control.lock().map_err(|_| Error::internal_error())?.interaction_epoch;
                    let scope = match &mut request.mode {
                        v2::ElicitationMode::Form(mode) => &mut mode.scope,
                        v2::ElicitationMode::Url(mode) => &mut mode.scope,
                        _ => return responder.respond(v2::CreateElicitationResponse::new(v2::ElicitationAction::Cancel)),
                    };
                    if let v2::ElicitationScope::Session(scope) = scope {
                        if downstream.lock().map_err(|_| Error::internal_error())?.as_ref() != Some(&scope.session_id) {
                            return responder.respond(v2::CreateElicitationResponse::new(v2::ElicitationAction::Cancel));
                        }
                        scope.session_id = hive_id.clone();
                    }
                    // Completion may be the next inbound message, before the spawned waiter runs.
                    if !interactions::announce_url(&control, &request) {
                        return responder.respond(v2::CreateElicitationResponse::new(v2::ElicitationAction::Cancel));
                    }
                    let control = control.clone();
                    let cancellation = responder.cancellation();
                    cx.spawn(async move {
                        let result = cancellation.run_until_cancelled(async { Ok(interactions::elicitation_at(control, request, epoch).await) }).await;
                        let _ = responder.respond_with_result(result);
                        Ok(())
                    })
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification({
            let control = control.clone();
            async move |notification: v2::CompleteElicitationNotification, _: V2ConnectionTo<Agent>| {
                interactions::complete(&control, notification);
                Ok(())
            }
        }, agent_client_protocol::on_receive_notification!())
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
            let (opened, downstream_id) = if let Some(id) = recovering {
                let id = v2::SessionId::new(id);
                // Route updates sent during session/resume, before its response is available.
                if let Ok(mut route) = downstream.lock() { *route = Some(id.clone()); }
                match cx.send_request(v2::ResumeSessionRequest::new(id.clone(), request.cwd)).block_task().await {
                    Ok(response) => (v2::NewSessionResponse::new(hive_id.clone()).config_options(response.config_options), id),
                    Err(error) => {
                        tracing::warn!(code = ?error.code, "agent session/resume failed");
                        let missing = error.data.as_ref().and_then(serde_json::Value::as_str)
                            .is_some_and(|data| data.contains("unknown session") || data.contains("not found"));
                        let detail = if missing { "saved session not found" } else { "session/resume failed" };
                        let _ = ready.send(Err(format!("agent {detail} ({:?}); verify its saved session and restart Hive", error.code)));
                        return Ok(());
                    }
                }
            } else {
                let prepared = cx.prepare_request(request);
                let parent = control.lock().map_err(|_| Error::internal_error())?.startup_parent.take();
                let prepared = if let Some(parent) = &parent { prepared.forward_cancellation_from(parent.cancellation.clone()) } else { prepared };
                let _mapping = interactions::ParentMapping::new(&control, prepared.id().clone(), parent);
                match prepared.block_task().await {
                    Ok(response) => { let id = response.session_id.clone(); (response, id) }
                    Err(error) => {
                        tracing::warn!(code = ?error.code, "agent session/new failed");
                        let _ = ready.send(Err("agent session/new failed; check its capabilities and configuration".into()));
                        return Ok(());
                    }
                }
            };
            if let Ok(mut route) = downstream.lock() {
                *route = Some(downstream_id.clone());
            }
            if let Ok(mut state) = control.lock() {
                state.selected_model = selected_model(&opened.config_options);
                state.config_options = opened.config_options.clone();
            }
            let _ = ready.send(Ok((opened, downstream_id.to_string())));
            if active.await.is_err() { return Ok(()); }
            while let Some(command) = rx.recv().await {
                match command {
                    Command::Configure(mut change) => {
                        change.request.session_id = downstream_id.clone();
                        let pending = {
                            let state = control.lock().map_err(|_| Error::internal_error())?;
                            if state.generation != change.generation {
                                drop(state);
                                change.finish(&hive_id, Err(Error::invalid_request().data("controller was superseded; take control before changing configuration")));
                                continue;
                            }
                            // Publish under the ownership lock, establishing dispatch-before-takeover ordering.
                            cx.send_request(change.request.clone()).block_task()
                        };
                        let result = pending.await.map(|response| response.config_options).map_err(|_| session_config::failed());
                        change.finish(&hive_id, result);
                    }
                    Command::Prompt(mut request, generation, key) => {
                        request.session_id = downstream_id.clone();
                        let prompt = {
                            let mut state = control.lock().expect("session state is not poisoned");
                            if state.generation != generation {
                                drop(state);
                                complete(&control, &key, Err("controller was superseded before prompt dispatch; resume with a new key".into()));
                                if let Ok(mut state) = control.lock() { state.running = false; state.notify(); }
                                continue;
                            }
                            // Publication under the lock defines the dispatch-before-handoff order.
                            let prepared = cx.prepare_request(request);
                            let parent = state.prompt_parents.remove(&key);
                            let prepared = if let Some(parent) = &parent { prepared.forward_cancellation_from(parent.cancellation.clone()) } else { prepared };
                            if let Some(parent) = parent { state.parents.insert(prepared.id().clone(), parent); }
                            let mapping = interactions::ParentMapping::registered(&control, prepared.id().clone());
                            (prepared.block_task(), mapping)
                        };
                        let (prompt, _mapping) = prompt;
                        tokio::pin!(prompt);
                        let result = loop {
                            tokio::select! {
                                result = &mut prompt => break result,
                                incoming = rx.recv() => match incoming {
                                    Some(Command::Cancel(epoch)) if epoch == 0 || current(&control, epoch) => { let _ = cx.send_notification(v2::CancelSessionNotification::new(downstream_id.clone())); }
                                    Some(Command::Cancel(_)) => {}
                                    Some(Command::Configure(change)) => change.finish(&hive_id, Err(session_config::failed())),
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
    startup: WorkerStartup,
    control: Arc<Mutex<Control>>,
    recovering: Option<String>,
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
                    let source = notification.update.clone();
                    match notification.update {
                        v1::SessionUpdate::ConfigOptionUpdate(update) => {
                            let options = session_config::from_v1(update.config_options)?;
                            forward(&control, &hive_id, v2::SessionUpdate::ConfigOptionUpdate(v2::ConfigOptionUpdate::new(options)))?;
                        }
                        v1::SessionUpdate::UsageUpdate(update) => {
                            report_v1_cost(&control, &hive_id, &update)?;
                        }
                        v1::SessionUpdate::AgentMessageChunk(chunk) => {
                            if let v1::ContentBlock::Text(text) = chunk.content {
                                let id = chunk_message_id.lock().map_err(|_| Error::internal_error())?.clone().ok_or_else(Error::internal_error)?;
                                forward(&control, &hive_id, v2::SessionUpdate::AgentMessageChunk(v2::ContentChunk::new(text.text.into(), id)))?;
                            }
                        }
                        v1::SessionUpdate::ToolCall(call) => {
                            let mut fields = BTreeMap::new();
                            fields.insert("tool".into(), serde_json::to_value(call).map_err(Error::into_internal_error)?);
                            forward_v1(&control, &hive_id, &source, v2::SessionUpdate::Other(v2::OtherSessionUpdate::new("_hive_v1_tool_call", fields)))?;
                        }
                        v1::SessionUpdate::ToolCallUpdate(call) => {
                            let mut fields = BTreeMap::new();
                            fields.insert("tool".into(), serde_json::to_value(call).map_err(Error::into_internal_error)?);
                            forward_v1(&control, &hive_id, &source, v2::SessionUpdate::Other(v2::OtherSessionUpdate::new("_hive_v1_tool_update", fields)))?;
                        }
                        v1::SessionUpdate::Plan(plan) => {
                            let mut fields = BTreeMap::new();
                            fields.insert("plan".into(), serde_json::to_value(plan).map_err(Error::into_internal_error)?);
                            forward_v1(&control, &hive_id, &source, v2::SessionUpdate::Other(v2::OtherSessionUpdate::new("_hive_v1_plan", fields)))?;
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
                    cx: ConnectionTo<Agent>| {
                    if route.lock().map_err(|_| Error::internal_error())?.as_ref() != Some(&request.session_id) {
                        return responder.respond(v1::RequestPermissionResponse::new(v1::RequestPermissionOutcome::Cancelled));
                    }
                    let options = request.options.iter().map(|option| {
                        serde_json::from_value::<v2::PermissionOption>(serde_json::to_value(option).map_err(Error::into_internal_error)?)
                            .map_err(Error::into_internal_error)
                    }).collect::<agent_client_protocol::Result<Vec<_>>>()?;
                    let title = request.tool_call.fields.title.clone().unwrap_or_else(|| "Agent tool permission".into());
                    let subject: v2::RequestPermissionSubject = serde_json::from_value(serde_json::json!({"type":"tool_call","toolCall":request.tool_call})).map_err(Error::into_internal_error)?;
                    let request = v2::RequestPermissionRequest::new(hive_id.clone(), title, options).subject(subject).meta(request.meta);
                    let control = control.clone();
                    let cancellation = responder.cancellation();
                    cx.spawn(async move {
                    let response = cancellation.run_until_cancelled(async { Ok(pilot_permission(control, request).await) }).await;
                    let Ok(response) = response else { let _ = responder.respond_with_error(Error::request_cancelled()); return Ok(()); };
                    let outcome = serde_json::from_value(serde_json::to_value(response.outcome).map_err(Error::into_internal_error)?)
                        .unwrap_or(v1::RequestPermissionOutcome::Cancelled);
                    let _ = responder.respond(v1::RequestPermissionResponse::new(outcome));
                    Ok(())
                    })
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
                    cx: ConnectionTo<Agent>| {
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
                    let control = control.clone();
                    let cancellation = responder.cancellation();
                    cx.spawn(async move {
                    let response = cancellation.run_until_cancelled(async { Ok(pilot_elicitation(control, request).await) }).await;
                    let Ok(response) = response else { let _ = responder.respond_with_error(Error::request_cancelled()); return Ok(()); };
                    let response: v1::CreateElicitationResponse = serde_json::from_value(
                        serde_json::to_value(response).map_err(Error::into_internal_error)?,
                    ).map_err(Error::into_internal_error)?;
                    let _ = responder.respond(response);
                    Ok(())
                    })
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(agent, async move |cx| {
            let initialized = cx
                .send_request(v1::InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await;
            let capabilities = match initialized {
                Ok(response) if response.protocol_version == ProtocolVersion::V1 => response.agent_capabilities,
                Ok(_) => {
                    let _ = ready.send(Err("agent negotiated a different protocol version; correct the profile's versions setting".into()));
                    return Ok(());
                }
                Err(error) => {
                    tracing::warn!(code = ?error.code, "v1 initialize failed");
                    let _ = ready.send(Err("v1 initialize failed; check the declared protocol version and agent availability".into()));
                    return Ok(());
                }
            };
            if !request.mcp_servers.is_empty() {
                let _ = ready.send(Err(
                    "v1 adapter cannot translate v2 MCP server declarations".into(),
                ));
                return Ok(());
            }
            let (downstream_id, options) = if let Some(id) = recovering {
                let id = v1::SessionId::new(id);
                if let Ok(mut route) = route.lock() { *route = Some(id.clone()); }
                let result = if capabilities.session_capabilities.resume.is_some() {
                    cx.send_request(v1::ResumeSessionRequest::new(id.clone(), request.cwd.0)).block_task().await.map(|response| response.config_options.unwrap_or_default())
                } else if capabilities.load_session {
                    cx.send_request(v1::LoadSessionRequest::new(id.clone(), request.cwd.0)).block_task().await.map(|response| response.config_options.unwrap_or_default())
                } else {
                    let _ = ready.send(Err("agent does not advertise ACPv1 session/resume or session/load; saved session is orphaned".into()));
                    return Ok(());
                };
                let options = match result {
                    Ok(options) => options,
                    Err(error) => {
                    tracing::warn!(code = ?error.code, "v1 agent reattachment failed");
                    let missing = error.data.as_ref().and_then(serde_json::Value::as_str)
                        .is_some_and(|data| data.contains("unknown session") || data.contains("not found"));
                    let detail = if missing { "saved session not found" } else { "reattachment failed" };
                    let _ = ready.send(Err(format!("v1 agent {detail} ({:?}); verify its saved session and restart Hive", error.code)));
                    return Ok(());
                    }
                };
                (id, options)
            } else {
                match cx.send_request(v1::NewSessionRequest::new(request.cwd.0)).block_task().await {
                    Ok(response) => (response.session_id, response.config_options.unwrap_or_default()),
                    Err(error) => {
                        tracing::warn!(code = ?error.code, "v1 session/new failed");
                        let _ = ready.send(Err("v1 session/new failed; check agent availability".into()));
                        return Ok(());
                    }
                }
            };
            if let Ok(mut route) = route.lock() {
                *route = Some(downstream_id.clone());
            }
            let options = session_config::from_v1(options)?;
            if let Ok(mut state) = control.lock() {
                state.selected_model = selected_model(&options);
                state.config_options = options.clone();
            }
            let _ = ready.send(Ok((v2::NewSessionResponse::new(hive_id.clone()).config_options(options), downstream_id.to_string())));
            if active.await.is_err() { return Ok(()); }
            while let Some(command) = rx.recv().await {
                match command {
                    Command::Configure(change) => {
                        let request = session_config::to_v1(&change.request, &downstream_id)?;
                        let pending = {
                            let state = control.lock().map_err(|_| Error::internal_error())?;
                            if state.generation != change.generation {
                                drop(state);
                                change.finish(&hive_id, Err(Error::invalid_request().data("controller was superseded; take control before changing configuration")));
                                continue;
                            }
                            cx.send_request(request).block_task()
                        };
                        let result = match pending.await {
                            Ok(response) => session_config::from_v1(response.config_options),
                            Err(_) => Err(session_config::failed()),
                        };
                        change.finish(&hive_id, result);
                    }
                    Command::Prompt(request, generation, key) => {
                        if !current(&control, generation) {
                            complete(&control, &key, Err("controller was superseded before prompt dispatch; resume with a new key".into()));
                            if let Ok(mut state) = control.lock() { state.running = false; state.notify(); }
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
                            if let Ok(mut state) = control.lock() { state.running = false; state.notify(); }
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
                                     Some(Command::Configure(change)) => change.finish(&hive_id, Err(session_config::failed())),
                                     Some(Command::Prompt(_, _, key)) => { complete(&control, &key, Err("session already has foreground work".into())); }
                                     Some(Command::Close(_, reply)) => { let _ = reply.send(Err("cancel active work before closing".into())); }
                                    None => break Err(Error::internal_error().data("client disconnected")),
                                }
                            }
                        };
                        let (stop, tokens) = match result {
                            Ok(response) => (match response.stop_reason {
                                v1::StopReason::EndTurn => v2::StopReason::EndTurn,
                                _ => v2::StopReason::Other(v2::OtherStopReason::new(
                                    "_v1_stop",
                                    Default::default(),
                                )),
                            }, response.usage),
                            Err(error) => {
                                tracing::warn!(code = ?error.code, "v1 prompt failed");
                                // Admission already succeeded: a replayable event carries late failure, not a second RPC response.
                                let failure = hive_protocol::AgentFailure::classify(&error.data.unwrap_or_default().to_string());
                                let _ = forward(&control, &hive_id, failure.update("acp-v1"));
                                (v2::StopReason::Other(v2::OtherStopReason::new(
                                    "_v1_error",
                                    Default::default(),
                                )), None)
                            }
                        };
                        let usage = tokens.map(|usage| v2::Usage::new(usage.total_tokens, usage.input_tokens, usage.output_tokens)
                            .thought_tokens(usage.thought_tokens)
                            .cached_read_tokens(usage.cached_read_tokens)
                            .cached_write_tokens(usage.cached_write_tokens));
                        let _ = forward(
                            &control,
                            &hive_id,
                            v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(
                                v2::IdleStateUpdate::new().stop_reason(stop).usage(usage),
                            )),
                        );
                        schedule_price(control.clone(), hive_id.clone());
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

/// Only the specified browser origin and its listener-local aliases may initiate an upgrade.
pub fn router_with_origin(broker: Broker, origin: Option<&str>) -> Router {
    let state = broker.clone();
    let agent = move || gateway_agent(state.clone());
    let mut server = AcpHttpServer::new(agent);
    if let Some(origin) = origin {
        // The SDK otherwise rejects browser WS upgrades that carry an Origin header.
        let mut origins = vec![origin.to_owned()];
        // Only aliases of this listener's loopback address and exact port are trusted browser origins.
        if let Some(address) = origin
            .strip_prefix("http://")
            .and_then(|address| address.parse::<SocketAddr>().ok())
            && address.ip().is_loopback()
        {
            origins.push(format!("http://localhost:{}", address.port()));
        }
        let cors = CorsOptions::allow_origins(origins)
            .expect("a loopback socket origin is a valid header");
        server = server.with_options(ServerOptions::default().with_cors(cors));
    }
    server
        .into_router()
        .route(
            "/v1/health",
            get(|| async { Json(serde_json::json!({"status":"ok"})) }),
        )
        .route(
            "/v1/ready",
            get(|| async { Json(serde_json::json!({"ready":true})) }),
        )
        .route(
            "/v1/status",
            get({
                let broker = broker.clone();
                move || {
                    let broker = broker.clone();
                    async move { Json(broker.status()) }
                }
            }),
        )
        .route(
            "/v1/providers",
            get({
                let broker = broker.clone();
                move || {
                    let broker = broker.clone();
                    async move { Json(broker.provider_info()) }
                }
            }),
        )
        .route(
            "/v1/providers/{agent}/native-sessions",
            get({
                let broker = broker.clone();
                move |axum::extract::Path(agent): axum::extract::Path<String>,
                      axum::extract::Query(query): axum::extract::Query<NativeQuery>| {
                    let broker = broker.clone();
                    async move {
                        broker
                            .native_sessions(&agent, query)
                            .await
                            .map(Json)
                    }
                }
            }),
        )
        .route(
            "/v1/sessions",
            get({
                let broker = broker.clone();
                move || {
                    let broker = broker.clone();
                    async move {
                        broker
                            .snapshot(None)
                            .map(Json)
                            .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)
                    }
                }
            }),
        )
        .route(
            "/v1/sessions/events",
            get({
                let broker = broker.clone();
                move || {
                    let catalog = broker.catalog.subscribe();
                    async move {
                        Sse::new(WatchStream::new(catalog).map(|revision| {
                            Ok::<Event, Infallible>(
                                Event::default()
                                    .event("revision")
                                    .id(revision.to_string())
                                    .data(revision.to_string()),
                            )
                        }))
                    }
                }
            }),
        )
        .route(
            "/ui",
            get(|| async { Html(include_str!("../../../clients/hive-web/index.html")) }),
        )
        .route(
            "/ui/app.js",
            get(|| async {
                (
                    [(axum::http::header::CONTENT_TYPE, "text/javascript")],
                    include_str!("../../../clients/hive-web/app.js"),
                )
            }),
        )
        .route(
            "/ui/icon.svg",
            get(|| async {
                (
                    [(axum::http::header::CONTENT_TYPE, "image/svg+xml")],
                    include_str!("../../../docs/images/icon.svg"),
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
            {
                let capabilities = broker.client_capabilities.clone();
                let client_id = client_id.clone();
                async move |request: v2::InitializeRequest,
                            responder: Responder<v2::InitializeResponse>,
                            cx: V2ConnectionTo<Client>| {
                    {
                        let mut capabilities = capabilities.lock().map_err(|_| Error::internal_error())?;
                        if capabilities.contains_key(&client_id) { return responder.respond_with_error(Error::invalid_request().data("connection is already initialized; reconnect to change capabilities")); }
                        capabilities.insert(client_id.clone(), request.capabilities.clone());
                    }
                    let capabilities = capabilities.clone();
                    let client_id = client_id.clone();
                    let connection = cx.clone();
                    cx.spawn(async move {
                        connection.incoming_closed().await;
                        if let Ok(mut capabilities) = capabilities.lock() {
                            capabilities.remove(&client_id);
                        }
                        Ok(())
                    })?;
                    responder.respond(
                        v2::InitializeResponse::new(
                            request.protocol_version,
                            v2::Implementation::new("hive", env!("CARGO_PKG_VERSION")),
                        )
                        .capabilities(
                            v2::AgentCapabilities::new().session(v2::SessionCapabilities::new()),
                        ),
                    )
                }
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
                    let broker = broker.clone();
                    let client_id = client_id.clone();
                    let connection = cx.clone();
                    let capabilities = broker
                        .client_capabilities
                        .lock()
                        .map_err(|_| Error::internal_error())?
                        .get(&client_id)
                        .cloned()
                        .unwrap_or_default();
                    let (parent, end) =
                        interactions::Parent::new(&responder, connection.clone(), capabilities);
                    cx.spawn(async move {
                        let _end = end;
                        let result = broker.create(request, client_id, connection, parent).await;
                        let _ = responder.respond_with_result(result);
                        Ok(())
                    })
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
                            cx: V2ConnectionTo<Client>| {
                    let capabilities = broker
                        .client_capabilities
                        .lock()
                        .map_err(|_| Error::internal_error())?
                        .get(&client_id)
                        .cloned()
                        .unwrap_or_default();
                    let (parent, end) =
                        interactions::Parent::new(&responder, cx.clone(), capabilities);
                    let received = broker.prompt(&client_id, request, parent)?;
                    cx.spawn(async move {
                        let _end = end;
                        let result = match received.await {
                            Ok(Ok(response)) => responder.respond(response),
                            Ok(Err(error)) => {
                                responder.respond_with_error(Error::internal_error().data(error))
                            }
                            Err(_) => responder.respond_with_error(
                                Error::internal_error().data("agent disconnected during prompt"),
                            ),
                        };
                        let _ = result;
                        Ok(())
                    })
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
                    let snapshot = broker.snapshot(request.cwd.as_ref())?;
                    let entries = snapshot
                        .sessions
                        .into_iter()
                        .map(|summary| {
                            let mut meta = v2::Meta::new();
                            let mut hive = serde_json::to_value(&summary)
                                .map_err(Error::into_internal_error)?;
                            hive["running"] = serde_json::json!(summary.activity == "running");
                            hive["pilot"] = serde_json::json!(summary.control_owner.is_some());
                            meta.insert("hive".into(), hive);
                            Ok(v2::SessionInfo::new(
                                v2::SessionId::new(summary.session_id),
                                summary.cwd,
                            )
                            .meta(meta))
                        })
                        .collect::<agent_client_protocol::Result<Vec<_>>>()?;
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
                async move |mut request: v2::SetSessionConfigOptionRequest,
                            responder: Responder<v2::SetSessionConfigOptionResponse>,
                            cx: V2ConnectionTo<Client>| {
                    let entry = broker.entry(&request.session_id)?;
                    let (reply, received) = oneshot::channel();
                    let mut state = entry.control.lock().map_err(|_| Error::internal_error())?;
                    state.owner(&client_id, request.meta.as_ref())?;
                    if state.running || state.closing || state.configuring || state.recovery_required || state.external_activity {
                        return responder.respond_with_error(Error::invalid_request().data("wait for idle and resolve recovery before changing configuration"));
                    }
                    if !session_config::advertised(&state.config_options, &request) {
                        return responder.respond_with_error(Error::invalid_params().data("this backend does not advertise that configuration option"));
                    }
                    // Reserve before enqueueing, otherwise a prompt could use the previous model while the setter is pending.
                    state.configuring = true;
                    let generation = state.generation;
                    if let Some(meta) = request.meta.as_mut() { meta.remove("hive"); }
                    let change = session_config::Change { request, generation, reply: Some(reply), control: entry.control.clone() };
                    let sent = entry.commands.send(Command::Configure(change));
                    drop(state);
                    // A failed send drops the reservation guard; never do that while holding its mutex.
                    if sent.is_err() { return responder.respond_with_error(Error::internal_error().data("agent disconnected; reopen the session")); }
                    cx.spawn(async move {
                        let result = received.await.unwrap_or_else(|_| Err(Error::internal_error().data("agent disconnected during configuration; reopen the session to inspect its current value")));
                        responder.respond_with_result(result)
                    })
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
                            cx: V2ConnectionTo<Client>| {
                    let entry = broker.entry(&request.session_id)?;
                    let (reply, received) = oneshot::channel();
                    {
                        let mut state =
                            entry.control.lock().map_err(|_| Error::internal_error())?;
                        state.owner(&client_id, request.meta.as_ref())?;
                        if state.running || state.closing || state.configuring {
                            return responder.respond_with_error(
                                Error::invalid_request()
                                    .data("cancel active work and wait for idle before closing"),
                            );
                        }
                        // Prevent takeover while a close has been admitted but not completed.
                        state.closing = true;
                        interactions::invalidate(&mut state);
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
                        state.notify();
                    }
                    let broker = broker.clone();
                    cx.spawn(async move {
                        match received.await {
                            Ok(Ok(())) => {
                                if let Some(store) = &broker.store {
                                    store
                                        .remove(&request.session_id.to_string())
                                        .map_err(storage_error)?;
                                }
                                let mut sessions = broker
                                    .sessions
                                    .lock()
                                    .map_err(|_| Error::internal_error())?;
                                if sessions.remove(&request.session_id.to_string()).is_some() {
                                    broker.catalog.send_modify(|revision| *revision += 1);
                                }
                                drop(sessions);
                                responder.respond(v2::CloseSessionResponse::new())
                            }
                            _ => {
                                if let Ok(mut state) = entry.control.lock() {
                                    state.closing = false;
                                    state.notify();
                                }
                                responder.respond_with_error(
                                    Error::internal_error().data("agent could not close session"),
                                )
                            }
                        }
                    })
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |request: v2::CancelSessionNotification, _cx: V2ConnectionTo<Client>| {
                let entry = broker.entry(&request.session_id)?;
                let mut state = entry.control.lock().map_err(|_| Error::internal_error())?;
                if state.owner(&client_id, request.meta.as_ref()).is_err() {
                    // JSON-RPC notifications have no response channel; never forward a stale cancel.
                    tracing::debug!("ignored cancel from stale Hive controller");
                    return Ok(());
                }
                interactions::invalidate(&mut state);
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
    let broker = Broker::persistent(
        registry,
        persistence::default_path().map_err(std::io::Error::other)?,
    )
    .map_err(std::io::Error::other)?;
    // Discovery stays responsive while slow or unavailable agents are reconciled.
    let recovery = tokio::spawn({
        let broker = broker.clone();
        async move { broker.reconcile().await }
    });
    let (shutdown_done, shut_down) = oneshot::channel();
    let server = axum::serve(listener, router_with_origin(broker.clone(), Some(&origin)))
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            recovery.abort();
            let _ = recovery.await;
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

/// Parent of the Hive runtime directory. Shared with `hive doctor` so that the two can never
/// disagree about where the lock lives.
pub(crate) fn runtime_base() -> std::io::Result<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .ok_or_else(|| std::io::Error::other("set XDG_RUNTIME_DIR or HOME to locate the Hive lock"))
}

fn instance_lock() -> std::io::Result<File> {
    let base = runtime_base()?;
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

    #[tokio::test(start_paused = true)]
    async fn startup_waits_for_human_input_but_still_times_out_a_silent_agent() {
        let control = Arc::new(Mutex::new(detached()));
        control.lock().unwrap().pending_interactions = 1;
        let ready = async {
            tokio::time::sleep(std::time::Duration::from_secs(20)).await;
            "ready"
        };
        assert_eq!(interactions::startup(&control, ready).await, Ok("ready"));
        control.lock().unwrap().pending_interactions = 0;
        let started = tokio::time::Instant::now();
        assert!(
            interactions::startup(&control, std::future::pending::<()>())
                .await
                .is_err()
        );
        assert_eq!(started.elapsed(), std::time::Duration::from_secs(15));
    }

    #[tokio::test(start_paused = true)]
    async fn an_unanswered_permission_expires_without_retaining_a_pending_waiter() {
        let mut state = detached();
        state.pilot = None;
        let control = Arc::new(Mutex::new(state));
        let started = tokio::time::Instant::now();
        let response = interactions::permission(
            control.clone(),
            v2::RequestPermissionRequest::new(v2::SessionId::new("session"), "Run?", vec![]),
        )
        .await;
        assert!(matches!(
            response.outcome,
            v2::RequestPermissionOutcome::Cancelled
        ));
        assert_eq!(started.elapsed(), std::time::Duration::from_secs(120));
        assert_eq!(control.lock().unwrap().pending_interactions, 0);
    }

    #[tokio::test]
    async fn cancellation_before_a_spawned_waiter_starts_cannot_revive_the_old_interaction() {
        let control = Arc::new(Mutex::new(detached()));
        let pending = interactions::permission(
            control.clone(),
            v2::RequestPermissionRequest::new(v2::SessionId::new("session"), "Run?", vec![]),
        );
        interactions::invalidate(&mut control.lock().unwrap());
        let response = pending.await;
        assert!(matches!(
            response.outcome,
            v2::RequestPermissionOutcome::Cancelled
        ));
        assert_eq!(control.lock().unwrap().pending_interactions, 0);
    }

    fn detached() -> Control {
        let (changed, _) = watch::channel(1);
        let (catalog, _) = watch::channel(0);
        Control {
            generation: 1,
            parents: HashMap::new(),
            startup_parent: None,
            prompt_parents: HashMap::new(),
            client_capabilities: HashMap::new(),
            interaction_epoch: 0,
            pending_interactions: 0,
            urls: HashMap::new(),
            early_completions: HashMap::new(),
            token: "first".into(),
            pilot: Some("test".into()),
            subscribers: HashMap::new(),
            changed,
            next_seq: 1,
            events: VecDeque::new(),
            running: false,
            activity_known: false,
            title: None,
            last_activity_at: None,
            catalog,
            closing: false,
            prompts: HashMap::new(),
            activity: Activity::new(),
            recovery_required: false,
            reattached: false,
            store: None,
            record: None,
            usage: Ledger::default(),
            active_turn: None,
            agent: "test".into(),
            pricing: Pricing::default(),
            selected_model: None,
            config_options: Vec::new(),
            configuring: false,
            external_activity: false,
            native: None,
        }
    }

    #[test]
    fn a_reported_session_cost_does_not_duplicate_a_turn_or_replace_its_tokens() {
        let mut state = detached();
        let id = v2::SessionId::new("hive-usage");
        state.active_turn = Some("one".into());
        let idle = v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(
            v2::IdleStateUpdate::new().usage(v2::Usage::new(12, 8, 4)),
        ));
        forward_locked(&mut state, &id, Some(idle.clone())).unwrap();
        forward_locked(&mut state, &id, Some(idle)).unwrap();
        assert_eq!(state.usage.turns.len(), 1);
        forward_locked(
            &mut state,
            &id,
            Some(v2::SessionUpdate::UsageUpdate(
                v2::UsageUpdate::new(12, 200_000).cost(v2::Cost::new(0.003, "USD")),
            )),
        )
        .unwrap();
        assert_eq!(state.usage.summary().turns, 1);
        assert_eq!(state.usage.summary().tokens.total_tokens, Some(12));
        assert_eq!(
            state.usage.summary().actual_session_cost.unwrap().amount,
            0.003
        );
        assert_eq!(state.usage.summary().context_used, Some(12));
        assert!(state.events.iter().any(|(_, event)| matches!(event,
            v2::SessionUpdate::Other(other) if other.session_update == "_hive_usage" && other.fields["turn"].is_null()
        )));
    }

    #[test]
    fn ambiguous_model_options_do_not_choose_a_billable_identity() {
        let option = |id: &str| {
            v2::SessionConfigOption::select(
                id,
                "Model",
                "model-a",
                vec![v2::SessionConfigSelectOption::new("model-a", "Model A")],
            )
            .category(v2::SessionConfigOptionCategory::Model)
        };
        assert_eq!(selected_model(&[option("model")]), Some("model-a".into()));
        assert_eq!(selected_model(&[option("first"), option("second")]), None);
        assert_eq!(selected_model(&[]), None);
    }

    #[test]
    fn an_idle_update_after_restart_does_not_acknowledge_an_interrupted_prompt() {
        let mut state = detached();
        state.recovery_required = true;
        forward_locked(
            &mut state,
            &v2::SessionId::new("hive-recovered"),
            Some(v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(
                v2::IdleStateUpdate::new(),
            ))),
        )
        .unwrap();
        assert!(state.recovery_required);
    }

    #[tokio::test]
    async fn an_unreported_agent_state_remains_unknown_until_a_status_update_arrives() {
        let (commands, _) = mpsc::unbounded_channel();
        let worker = tokio::spawn(async {});
        let entry = Entry {
            cwd: v2::AbsolutePath::new(std::env::current_dir().unwrap()),
            profile: "mock".into(),
            abort: worker.abort_handle(),
            commands,
            control: Arc::new(Mutex::new(detached())),
        };
        let mut state = entry.control.lock().unwrap();
        let initial = SessionSummary::from_entry("hive-test", &entry, &state);
        assert_eq!(initial.activity, "unknown");
        assert_eq!(initial.last_activity_at, None);
        assert_eq!(initial.control_owner.as_deref(), Some("test"));
        forward_locked(
            &mut state,
            &v2::SessionId::new("hive-test"),
            Some(v2::SessionUpdate::StateUpdate(v2::StateUpdate::Running(
                v2::RunningStateUpdate::new(),
            ))),
        )
        .unwrap();
        let running = SessionSummary::from_entry("hive-test", &entry, &state);
        assert_eq!(running.activity, "running");
        assert!(running.last_activity_at.is_some());
        forward_locked(
            &mut state,
            &v2::SessionId::new("hive-test"),
            Some(v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(
                v2::IdleStateUpdate::new(),
            ))),
        )
        .unwrap();
        let idle = SessionSummary::from_entry("hive-test", &entry, &state);
        assert_eq!(idle.activity, "idle");
        assert!(idle.last_activity_at >= running.last_activity_at);
    }

    #[test]
    fn the_first_meaningful_prompt_text_becomes_a_bounded_title() {
        let blocks = vec![
            v2::ContentBlock::Text(v2::TextContent::new("  \n  ")),
            v2::ContentBlock::Text(v2::TextContent::new(format!(
                "  {}   task  ",
                "a".repeat(130)
            ))),
        ];
        let title = prompt_title(&blocks).unwrap();
        assert_eq!(title.chars().count(), 120);
        assert!(!title.contains(' '));
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
        assert_eq!(state.events.front().unwrap().0, 265);
        assert_eq!(state.next_seq, 521);
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
            std::collections::BTreeMap::from([("seq".into(), serde_json::json!(265))]),
        ));
        assert_eq!(replay_start(&state, Some(&available)).unwrap(), Some(265));
        assert_eq!(
            state.generation, 1,
            "a failed cursor cannot take over control"
        );
    }

    #[test]
    fn expired_history_retains_current_activity_for_a_live_only_attachment() {
        let state = Mutex::new(detached());
        let id = v2::SessionId::new("hive-test");
        forward(
            &state,
            &id,
            v2::SessionUpdate::ToolCallUpdate(
                v2::ToolCallUpdate::new("finished")
                    .status(v2::ToolCallStatus::Failed)
                    .raw_output(serde_json::json!({"error": "failure"})),
            ),
        )
        .unwrap();
        for _ in 0..260 {
            forward(
                &state,
                &id,
                v2::SessionUpdate::AgentMessageChunk(v2::ContentChunk::new(
                    "text".into(),
                    v2::MessageId::new("message"),
                )),
            )
            .unwrap();
        }
        let state = state.lock().unwrap();
        assert!(
            replay_start(
                &state,
                Some(&v2::ReplayFrom::Start(v2::ReplayFromStart::new()))
            )
            .is_err()
        );
        assert!(replay_start(&state, None).unwrap().is_none());
        let snapshot = serde_json::to_value(&state.activity).unwrap();
        assert_eq!(snapshot["tools"]["finished"]["status"], "failed");
        assert_eq!(
            snapshot["tools"]["finished"]["rawOutput"]["error"],
            "failure"
        );
        assert_eq!(snapshot["seq"], 2);
    }
}
