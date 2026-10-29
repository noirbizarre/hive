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
    AcpAgent, AcpAgentConfig, Agent, Client, ConnectTo, ConnectionTo, Error, PreparedRequest,
    Responder, V2ConnectionTo,
};
use agent_client_protocol_http::{CorsOptions, HttpClient, ServerOptions};
use axum::{
    Json, Router,
    response::{
        Html,
        sse::{Event, Sse},
    },
    routing::{get, post},
};
use fs2::FileExt;
use hive_core::SessionId;
use hive_protocol::history::{
    Availability, Completeness, Restoration, Source, TranscriptStatus, WarningCode,
};
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
mod plans;
mod pricing;
pub mod projects;
mod prompt_journal;
pub mod reload;
mod session_config;
mod transcript;
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
    registry: Arc<Mutex<reload::Registry>>,
    reloading: Arc<tokio::sync::Mutex<()>>,
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

type NativeInterruption = watch::Sender<Option<tokio::time::Instant>>;

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
    transcript: transcript::Transcript,
    // Recovery replay must not run accounting or dispatch historical interactions.
    hydrating: bool,
    history_error: Option<transcript::RestoreError>,
    history_status: TranscriptStatus,
    require_history_state: bool,
    expected_history: bool,
    // Admission and resume barriers share native evidence so blocked work can never appear idle.
    current_state: Option<v2::StateUpdate>,
    // Only the matching undispatched prompt may restore its previous foreground evidence.
    foreground_reservation: Option<String>,
    title: Option<String>,
    last_activity_at: Option<u64>,
    catalog: watch::Sender<u64>,
    closing: bool,
    // Native v2 dialog replies share the close-admission fence with the worker's watchdog.
    native_interruption: Option<NativeInterruption>,
    prompts: HashMap<String, PromptRecord>,
    activity: Activity,
    plans: plans::Plans,
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
    legacy_config: session_config::Legacy,
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
    fingerprint: prompt_journal::Fingerprint,
    dispatched: bool,
    saw_work: bool,
    result: Option<Result<v2::PromptResponse, PromptFailure>>,
    waiters: Vec<oneshot::Sender<Result<v2::PromptResponse, PromptFailure>>>,
}

#[derive(Clone, Debug, thiserror::Error)]
pub(crate) enum PromptFailure {
    #[error("prompt cancelled before insertion")]
    Cancelled,
    #[error("prompt was not dispatched; submit new intent with a new key")]
    NotDispatched,
    #[error("cannot persist prompt evidence; inspect agent state and acknowledge recovery")]
    Storage,
    #[error("prompt outcome remains uncertain; this key will not execute again")]
    Uncertain { recovery_required: bool },
    #[error("{0}")]
    Failed(String),
}

impl From<String> for PromptFailure {
    fn from(message: String) -> Self {
        Self::Failed(message)
    }
}

impl From<&str> for PromptFailure {
    fn from(message: &str) -> Self {
        Self::Failed(message.into())
    }
}

impl PromptFailure {
    fn wire(self) -> Error {
        match self {
            Self::Cancelled => Error::request_cancelled(),
            Self::NotDispatched => {
                hive_protocol::recovery::RecoveryError::NotDispatched.wire(false)
            }
            Self::Storage => hive_protocol::recovery::RecoveryError::Storage.wire(true),
            Self::Failed(_) => hive_protocol::recovery::RecoveryError::Uncertain.wire(true),
            Self::Uncertain { recovery_required } => {
                hive_protocol::recovery::RecoveryError::Uncertain.wire(recovery_required)
            }
        }
    }
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
    fn begin_close(&mut self) -> agent_client_protocol::Result<()> {
        if self.closing {
            return Err(Error::invalid_request().data("session close is already in progress"));
        }
        // Fence late-registering dialogs atomically with close admission, before the worker runs.
        begin_native_interruption(self);
        self.closing = true;
        Ok(())
    }

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
            transcript: transcript::Transcript::default(),
            hydrating: true,
            history_error: None,
            history_status: TranscriptStatus {
                availability: Availability::Loading,
                ..TranscriptStatus::default()
            },
            require_history_state: false,
            expected_history: !record.usage.turns.is_empty(),
            current_state: None,
            foreground_reservation: None,
            title: record.title.clone(),
            last_activity_at: record.last_activity_at,
            catalog,
            closing: false,
            native_interruption: None,
            prompts: HashMap::new(),
            activity,
            plans: plans::Plans::default(),
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
            legacy_config: session_config::Legacy::default(),
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
            transcript: transcript::Transcript::default(),
            hydrating: false,
            history_error: None,
            history_status: TranscriptStatus {
                source: Source::Live,
                availability: Availability::Available,
                ..TranscriptStatus::default()
            },
            require_history_state: false,
            expected_history: false,
            current_state: None,
            foreground_reservation: None,
            title: None,
            last_activity_at: None,
            catalog,
            closing: false,
            native_interruption: None,
            prompts: HashMap::new(),
            activity,
            plans: plans::Plans::default(),
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
            legacy_config: session_config::Legacy::default(),
            configuring: false,
            external_activity: false,
            native: None,
        }
    }

    fn metadata(&self, client_id: &str) -> v2::Meta {
        let mut meta = v2::Meta::new();
        let mut activity = serde_json::to_value(&self.activity).expect("activity is serializable");
        let foreground = self.foreground();
        activity["foreground"] = foreground.into();
        // Live-only clients skip pre-response events; the response must cover the same state.
        activity["seq"] = self.next_seq.saturating_sub(1).into();
        let mut hive = serde_json::json!({
            "generation": self.generation, "nextSeq": self.next_seq,
            "running": self.busy(), "pilot": self.pilot.is_some(),
            "activitySnapshot": activity,
            "planSnapshot": self.plans.snapshot(self.next_seq.saturating_sub(1)),
            "usageSnapshot": self.usage.summary(),
            "usageSeq": self.next_seq.saturating_sub(1),
            "configSeq": self.next_seq.saturating_sub(1),
            "transcript": self.history_status,
            "transcriptSeq": self.next_seq.saturating_sub(1),
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

    fn finish_history(&mut self) {
        if self.hydrating && self.history_status.restoration == Restoration::Supported {
            self.history_status.source = Source::AgentHistory;
            self.history_status.availability = Availability::Available;
        }
        // Completed work is evidence for a warning, not proof that the agent must retain it.
        if self.expected_history && !self.transcript.has_messages() && self.history_error.is_none()
        {
            self.history_status.warn(WarningCode::HistoryMissing);
            tracing::warn!(
                diagnostic_code = "hive::transcript::history_missing",
                "agent resumed without retained conversation messages"
            );
        }
        if self.require_history_state && self.foreground() == "unknown" {
            self.history_status.warn(WarningCode::StateMissing);
            tracing::warn!(
                diagnostic_code = "hive::transcript::state_missing",
                "resume response omitted current state"
            );
        }
        self.require_history_state = false;
        self.hydrating = false;
    }

    fn status_update(&self) -> v2::SessionUpdate {
        v2::SessionUpdate::Other(v2::OtherSessionUpdate::new(
            "_hive_control",
            BTreeMap::from([
                ("generation".into(), serde_json::json!(self.generation)),
                ("pilot".into(), serde_json::json!(self.pilot.is_some())),
                (
                    "recoveryRequired".into(),
                    serde_json::json!(self.recovery_required),
                ),
                ("transcript".into(), serde_json::json!(self.history_status)),
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

    fn busy(&self) -> bool {
        // Blocked work remains busy even without a preceding Running event.
        matches!(self.foreground(), "running" | "requires_action")
    }

    fn foreground(&self) -> &'static str {
        if self.foreground_reservation.is_some() {
            return "running";
        }
        match self.current_state {
            Some(v2::StateUpdate::Running(_)) => "running",
            Some(v2::StateUpdate::Idle(_)) => "idle",
            Some(v2::StateUpdate::RequiresAction(_)) => "requires_action",
            _ => "unknown",
        }
    }

    fn resume_state(&self) -> Option<v2::StateUpdate> {
        if self.foreground_reservation.is_some() {
            return Some(v2::StateUpdate::Running(v2::RunningStateUpdate::new()));
        }
        let mut status = self.current_state.clone()?;
        if let v2::StateUpdate::Idle(idle) = &mut status {
            // Synchronization is not another completion, cancellation, failure or billable turn.
            idle.stop_reason = None;
            idle.usage = None;
        }
        Some(status)
    }

    fn observe_state(&mut self, status: &v2::StateUpdate) {
        self.foreground_reservation = None;
        self.current_state = matches!(
            status,
            v2::StateUpdate::Running(_)
                | v2::StateUpdate::Idle(_)
                | v2::StateUpdate::RequiresAction(_)
        )
        .then(|| status.clone());
    }

    fn rollback_foreground(&mut self, key: &str) {
        if self
            .foreground_reservation
            .as_ref()
            .is_some_and(|reserved| reserved == key)
        {
            // Native evidence is retained while reserved, so rollback needs no second state copy.
            self.foreground_reservation = None;
            self.notify();
        }
    }

    fn finish(&mut self, key: &str, result: Result<v2::PromptResponse, PromptFailure>) {
        // Late shutdown failures must not overwrite an insertion already acknowledged.
        if self
            .prompts
            .get(key)
            .is_some_and(|record| record.result.is_some())
        {
            return;
        }
        if result.is_err()
            && self
                .foreground_reservation
                .as_ref()
                .is_some_and(|reserved| reserved == key)
        {
            // A failed native RPC may have started work; only a later state can resolve it.
            self.foreground_reservation = None;
            self.current_state = None;
            self.notify();
        }
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
    transcript: TranscriptStatus,
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
            activity: state.foreground(),
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
            transcript: state.history_status.clone(),
        }
    }

    fn from_record(record: &Record) -> Self {
        let mut transcript = TranscriptStatus::default();
        if matches!(record.outcome, Outcome::Recovering) {
            transcript.availability = Availability::Loading;
        }
        if record
            .reason
            .as_deref()
            .is_some_and(|reason| reason.starts_with("hive::transcript::operation_rejected:"))
        {
            transcript.warn(WarningCode::OperationRejected);
        }
        if record
            .reason
            .as_deref()
            .is_some_and(|reason| reason.starts_with("hive::transcript::unsupported:"))
        {
            transcript.warn(WarningCode::Unsupported);
        }
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
            transcript,
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
        // Full history has a separate snapshot boundary, not an incremental event cursor.
        Some(v2::ReplayFrom::Start(_)) => None,
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
                            && record.reason.as_deref().is_some_and(|reason| reason.contains("timed out") || reason.contains("reattachment failed") || reason.contains("session/resume failed") || reason.contains("hive::transcript::replay_failed")
                            // A server that is merely down comes back; a missing or unsupported one does not.
                            || reason.contains(opencode::error::code::UNREACHABLE) || reason.contains(opencode::error::code::STREAM_LOST)));
                    if !retry { break; }
                    if attempt == 2 {
                        if let Ok(mut offline) = broker.offline.lock() && let Some(record) = offline.get_mut(&id) {
                            record.reason = Some(format!("{}; agent recovery failed after three attempts; check agent state and restart Hive to retry", record.reason.as_deref().unwrap_or("agent unavailable")));
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
        let Some(provider) = self.config().agents.get(&record.profile).cloned() else {
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
        if provider.kind == ProfileKind::OpencodeServer
            && (record.identity.is_none() || record.identity != reload::identity(&provider))
        {
            record.outcome = Outcome::Unavailable;
            record.reason = Some("agent server identity changed; restore the original connection configuration and restart Hive, or use a new profile ID".into());
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
        let request = v2::NewSessionRequest::new(cwd.clone())
            .additional_directories(record.additional_directories.clone())
            .mcp_servers(record.mcp_servers.clone());
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
                // Live updates may already have changed usage/cursors after the ordered resume
                // response; publishing recovery must not overwrite them with the startup copy.
                let mut stored = saved.lock().expect("session record is not poisoned");
                stored.outcome = Outcome::Active;
                stored.reason = None;
                if let Err(error) = store.save(&stored) {
                    tracing::error!(%error, "cannot save reattached session");
                    worker.abort();
                    return;
                }
                record = stored.clone();
                drop(stored);
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
                    || error.contains("hive::transcript::unsupported")
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
            registry: Arc::new(Mutex::new(reload::Registry {
                managed_projects: Default::default(),
                current: Arc::new(registry),
                source: None,
                bindings: HashMap::new(),
                stopping: false,
            })),
            reloading: Arc::new(tokio::sync::Mutex::new(())),
            sessions: Arc::new(Mutex::new(HashMap::new())),
            catalog,
            store: None,
            offline: Arc::new(Mutex::new(HashMap::new())),
            adopting: Arc::new(Mutex::new(std::collections::HashSet::new())),
            client_capabilities: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn persistent(registry: Config, path: PathBuf) -> Result<Self, projects::InitError> {
        if let Some(problem) = registry.problems().into_iter().next() {
            return Err(projects::InitError::Config(problem.error));
        }
        let store = Arc::new(Store::open(path)?);
        let managed = store.projects()?;
        if let Some(id) = registry
            .projects
            .keys()
            .find(|id| managed.contains_key(*id))
        {
            return Err(projects::InitError::Collision(id.clone()));
        }
        store.recover_prompts()?;
        // Retain interrupted keys indefinitely; only definitely non-resumable old records age out.
        store.prune(now_millis().saturating_sub(90 * 24 * 60 * 60 * 1000))?;
        let mut broker = Self::new(registry);
        broker
            .registry
            .lock()
            .expect("configuration lock poisoned")
            .managed_projects = managed;
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
        {
            let mut registry = broker.registry.lock().expect("configuration lock poisoned");
            let mut offline = broker.offline.lock().expect("offline lock poisoned");
            for record in offline.values_mut() {
                let kind = if record.kind == ProfileKind::OpencodeServer.as_str() {
                    ProfileKind::OpencodeServer
                } else {
                    ProfileKind::Acp
                };
                // Legacy records did not carry identity: preserve the startup recovery baseline.
                if kind == ProfileKind::OpencodeServer
                    && record.identity.is_none()
                    && let Some(profile) = registry.current.agents.get(&record.profile)
                    && profile.kind == kind
                {
                    record.identity = reload::identity(profile);
                    store.save(record)?;
                }
                registry.bindings.insert(
                    record.id.clone(),
                    reload::Binding {
                        profile: record.profile.clone(),
                        kind,
                        identity: record.identity.clone(),
                    },
                );
            }
        }
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
    ) -> agent_client_protocol::Result<oneshot::Receiver<Result<v2::PromptResponse, PromptFailure>>>
    {
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
        // Compute before reserving admission so encoding failure cannot leave the session busy.
        let fingerprint = prompt_journal::Fingerprint::content(&request.prompt)
            .map_err(Error::into_internal_error)?;
        if let Some(record) = state.prompts.get_mut(&key) {
            if record.fingerprint != fingerprint {
                return Err(hive_protocol::recovery::RecoveryError::KeyConflict
                    .wire(state.recovery_required));
            }
            if let Some(result) = &record.result {
                let mut result = result.clone();
                if matches!(
                    result,
                    Err(PromptFailure::Failed(_)
                        | PromptFailure::Storage
                        | PromptFailure::Uncertain { .. })
                ) {
                    result = Err(PromptFailure::Uncertain {
                        recovery_required: state.recovery_required,
                    });
                }
                let _ = tx.send(result);
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
        let persisted = state
            .store
            .as_ref()
            .map(|store| store.prompt(&request.session_id.to_string(), &key))
            .transpose();
        let persisted = match persisted {
            Ok(value) => value.flatten(),
            Err(error) => {
                tracing::error!(%error, "cannot read prompt evidence");
                state.recovery_required = true;
                recovery_changed(&mut state);
                return Err(hive_protocol::recovery::RecoveryError::Storage.wire(true));
            }
        };
        if let Some((stored_fingerprint, result)) = persisted {
            if stored_fingerprint != fingerprint {
                return Err(hive_protocol::recovery::RecoveryError::KeyConflict
                    .wire(state.recovery_required));
            }
            if let Some(result) = result {
                let mut result = result.into_response();
                if matches!(
                    result,
                    Err(PromptFailure::Failed(_)
                        | PromptFailure::Storage
                        | PromptFailure::Uncertain { .. })
                ) {
                    result = Err(PromptFailure::Uncertain {
                        recovery_required: state.recovery_required,
                    });
                }
                let _ = tx.send(result);
                return Ok(rx);
            }
            return Err(
                hive_protocol::recovery::RecoveryError::Uncertain.wire(state.recovery_required)
            );
        }
        if state.recovery_required {
            return Err(hive_protocol::recovery::RecoveryError::Required.wire(true));
        }
        if state.external_activity {
            // Say *why* it is busy: the generic message would send users hunting for a Hive prompt.
            return Err(opencode::error::OpencodeError::ExternallyControlled.to_acp());
        }
        if state.busy() || state.closing || state.configuring {
            return Err(Error::invalid_request().data(
                "session has active foreground work; wait for idle or cancel before prompting",
            ));
        }
        // Reserving under the ownership lock prevents two simultaneous prompts from both starting.
        let previous_time = state.last_activity_at;
        state.foreground_reservation = Some(key.clone());
        record_activity(&mut state);
        let title = state
            .title
            .clone()
            .or_else(|| prompt_title(&request.prompt));
        state.prompts.insert(
            key.clone(),
            PromptRecord {
                fingerprint: fingerprint.clone(),
                dispatched: false,
                saw_work: false,
                result: None,
                waiters: vec![tx],
            },
        );
        state.active_turn = Some(key.clone());
        if let (Some(store), Some(record)) = (state.store.clone(), state.record.clone()) {
            let mut record = record.lock().map_err(|_| Error::internal_error())?;
            let mut next = record.clone();
            next.uncertain = true;
            next.last_activity_at = state.last_activity_at;
            next.title = title.clone();
            if let Err(error) = store.reserve_prompt(&next, &key, &fingerprint) {
                state.prompts.remove(&key);
                state.active_turn = None;
                state.rollback_foreground(&key);
                state.recovery_required = true;
                tracing::error!(%error, "cannot reserve prompt evidence");
                drop(record);
                recovery_changed(&mut state);
                return Err(hive_protocol::recovery::RecoveryError::Storage.wire(true));
            }
            *record = next;
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
            drop(state);
            complete(&entry.control, &key, Err(PromptFailure::NotDispatched));
            let mut state = entry.control.lock().map_err(|_| Error::internal_error())?;
            state.last_activity_at = previous_time;
            return Ok(rx);
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
        let snapshot = matches!(request.replay_from, Some(v2::ReplayFrom::Start(_)));
        if snapshot
            && state.history_status.availability != Availability::Available
            && let Some(error) = &state.history_error
        {
            return Err(Error::invalid_request().data(error.to_string()));
        }
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
            return Err(hive_protocol::recovery::RecoveryError::Required.wire(true));
        }
        // Keep replay and live forwarding in one critical section: no update can cross this boundary twice.
        let snapshot_through = state.next_seq.saturating_sub(1);
        if snapshot {
            for update in state
                .transcript
                .updates()?
                .into_iter()
                .chain(state.plans.updates())
            {
                let mut meta = event_meta(snapshot_through);
                meta.get_mut("hive").expect("event metadata")["snapshot"] = true.into();
                cx.send_notification(
                    v2::UpdateSessionNotification::new(request.session_id.clone(), update)
                        .meta(meta),
                )?;
            }
        }
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
                if action.is_some() {
                    store.acknowledge(&next).map_err(storage_error)?;
                } else {
                    store.save(&next).map_err(storage_error)?;
                }
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
        if let Some(status) = state.resume_state() {
            // Keep the barrier in the replay/live critical section without re-ingesting a turn.
            publish_locked(
                &mut state,
                &request.session_id,
                v2::SessionUpdate::StateUpdate(status),
            )?;
            if !state.subscribers.contains_key(client_id) {
                return Err(Error::internal_error().data(
                    "resume state could not be delivered; reconnect and resume the session",
                ));
            }
        }
        let mut meta = state.metadata(client_id);
        if snapshot {
            meta.get_mut("hive").expect("control metadata")["snapshotThroughSeq"] =
                snapshot_through.into();
        }
        let response = v2::ResumeSessionResponse::new()
            .config_options(state.config_options.clone())
            .meta(meta);
        let control = entry.control.clone();
        let id = request.session_id.clone();
        let subscriber = client_id.to_owned();
        watch_attachment(cx, control, id, subscriber);
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
            default_agent: self.config().default_agent.clone(),
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
        let registry = self.config();
        let profile = registry.agents.get(agent).ok_or_else(|| {
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
        let registry = self.config();
        serde_json::json!({ "defaultAgent": registry.default_agent, "agents": registry.public_profiles() })
    }

    /// Close every downstream worker when an embedded gateway stops serving requests.
    pub async fn shutdown(&self) {
        {
            // Let committed project writes finish publishing before shutdown closes admission.
            let _writer = self.reloading.lock().await;
            if let Ok(mut registry) = self.registry.lock() {
                registry.stopping = true;
            }
        }
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
            None => None,
        };
        let hive_id = v2::SessionId::new(SessionId::new().as_str());
        let (chosen, provider, mut reservation) = {
            // Selection and binding publication are one admission boundary with reload.
            let mut registry = self.registry.lock().map_err(|_| Error::internal_error())?;
            if registry.stopping {
                return Err(Error::invalid_request().data("Hive is shutting down"));
            }
            let chosen = chosen.or_else(|| registry.current.default_agent.clone());
            let chosen = chosen.ok_or_else(|| Error::invalid_params().data("no default agent configured; set default_agent in Hive config or pass _meta.hive.agent in session/new"))?;
            let provider = registry
                .current
                .agents
                .get(&chosen)
                .cloned()
                .ok_or_else(|| {
                    Error::invalid_params().data(format!(
                        "unknown agent profile '{chosen}'; query GET /v1/providers"
                    ))
                })?;
            registry
                .check(&chosen, &provider)
                .map_err(|error| Error::invalid_request().data(error.to_string()))?;
            registry.bindings.insert(
                hive_id.to_string(),
                reload::Binding {
                    profile: chosen.clone(),
                    kind: provider.kind,
                    identity: reload::identity(&provider),
                },
            );
            (
                chosen,
                provider,
                reload::Reservation {
                    registry: self.registry.clone(),
                    id: hive_id.to_string(),
                    retained: false,
                },
            )
        };
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
                additional_directories: request.additional_directories.clone(),
                mcp_servers: request.mcp_servers.clone(),
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
                identity: reload::identity(&provider),
                origin: if adopt.is_some() { "adopted" } else { "hive" }.into(),
            }))
        });
        let worker_record = stored_record.clone();
        let worker_registry = self.registry.clone();
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
            if store.is_none()
                && let Ok(mut registry) = worker_registry.lock()
            {
                // Keep the identity reserved until native-ID discovery can no longer find it.
                registry.bindings.remove(&id.to_string());
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
            watch_attachment(cx, control, id, subscriber);
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
                    // Once durable, cancellation must not forget the native identity.
                    reservation.retained = true;
                    let mut state = entry_control.lock().map_err(|_| Error::internal_error())?;
                    state.store = Some(store.clone());
                    state.record = stored_record.clone();
                }
                {
                    let mut sessions = self.sessions.lock().map_err(|_| Error::internal_error())?;
                    sessions.insert(hive_id.to_string(), entry);
                    self.catalog.send_modify(|revision| *revision += 1);
                }
                reservation.retained = true;
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

async fn restore_history(
    request: PreparedRequest<session_config::Discovery>,
    control: Arc<Mutex<Control>>,
) -> agent_client_protocol::Result<Vec<v2::SessionConfigOption>> {
    let (tx, rx) = oneshot::channel();
    // Install the barrier before publication: block_task alone lets post-response live
    // notifications overtake the task that ends historical ingestion.
    request.on_receiving_result(async move |result| {
        let result = result.and_then(|discovery| {
            let mut state = control.lock().map_err(|_| Error::internal_error())?;
            let options = state.legacy_config.discover(discovery)?;
            state.selected_model = selected_model(&options);
            state.config_options = options.clone();
            state.finish_history();
            Ok(options)
        });
        let _ = tx.send(result);
        Ok(())
    })?;
    rx.await.map_err(|_| Error::internal_error())?
}

fn forward_v1(
    control: &Mutex<Control>,
    id: &v2::SessionId,
    source: &v1::SessionUpdate,
    raw: v2::SessionUpdate,
) -> agent_client_protocol::Result<()> {
    let mut state = control.lock().map_err(|_| Error::internal_error())?;
    forward_locked(&mut state, id, Some(raw))?;
    if state.hydrating {
        // Old v1 tools are transcript entries, not current activity to publish again.
        if matches!(source, v1::SessionUpdate::Plan(_)) {
            state.activity.v1(source);
        }
        return Ok(());
    }
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

// Attachment snapshots share cursors with live updates, but must not repeat ingestion effects.
fn publish_locked(
    state: &mut Control,
    id: &v2::SessionId,
    update: v2::SessionUpdate,
) -> agent_client_protocol::Result<()> {
    let seq = state.next_seq;
    if let (Some(store), Some(record)) = (&state.store, &state.record) {
        let mut record = record.lock().map_err(|_| Error::internal_error())?;
        if seq > record.reserved_seq {
            // Reserve before publishing so a crash cannot reuse an exposed cursor.
            record.reserved_seq = seq.saturating_add(1023);
            store.save(&record).map_err(storage_error)?;
        }
    }
    state.next_seq += 1;
    if state.events.len() == 256 {
        state.events.pop_front();
    }
    state.events.push_back((seq, update.clone()));
    let subscribers = state.subscribers.len();
    // A broken observer cannot interrupt the downstream worker or other subscribers.
    state.subscribers.retain(|_, gateway| {
        gateway
            .send_notification(
                v2::UpdateSessionNotification::new(id.clone(), update.clone())
                    .meta(event_meta(seq)),
            )
            .is_ok()
    });
    if subscribers != state.subscribers.len() {
        state.notify();
    }
    Ok(())
}

fn forward_locked(
    state: &mut Control,
    id: &v2::SessionId,
    update: Option<v2::SessionUpdate>,
) -> agent_client_protocol::Result<()> {
    let is_agent_update = update.is_some();
    let update = update.unwrap_or_else(|| state.status_update());
    if !state.hydrating
        && !state.recovery_required
        && state.foreground_reservation.is_some()
        && matches!(
            &update,
            v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(_))
        )
        && !state
            .active_turn
            .as_ref()
            .and_then(|key| state.prompts.get(key))
            .is_some_and(|prompt| prompt.saw_work)
    {
        // Dispatch alone cannot correlate a delayed startup/previous-turn idle with this prompt.
        // Wait for running/requires_action so the snapshot cannot release admission or fence it.
        return Ok(());
    }
    // Token streaming must not clone warning strings on every chunk.
    let previous_history = (
        state.history_status.source,
        state.history_status.availability,
        state.history_status.completeness,
        state.history_status.warnings.len(),
    );
    let failure = state.transcript.ingest(&update)?;
    if let v2::SessionUpdate::PlanUpdate(plan) = &update {
        state.plans.ingest(plan);
        if state.hydrating {
            // Restore only the convenience view; historical plans must not publish live events.
            state.activity.v2(&update);
        }
    }
    if state.transcript.limited() {
        state.history_status.partial(WarningCode::ProjectionLimited);
    }
    if state.hydrating {
        if let Some(failure) = failure {
            // Restored diagnostics belong to history without allocating live cursors or usage.
            state.transcript.ingest(&failure)?;
        }
        // Resume's final state describes current execution; historical usage never enters the ledger.
        match &update {
            v2::SessionUpdate::StateUpdate(status) => {
                state.observe_state(status);
                state.activity.v2(&update);
            }
            v2::SessionUpdate::ConfigOptionUpdate(options) => {
                state.config_options = options.config_options.clone();
                state.selected_model = selected_model(&options.config_options);
            }
            _ => {}
        }
        return Ok(());
    }
    if state.history_status.availability != Availability::Available
        && state.transcript.has_messages()
    {
        // Context-only resume can accumulate a useful view without recovering the missing prefix.
        state.history_status.source = Source::Live;
        state.history_status.availability = Availability::Available;
        state.history_status.completeness = Completeness::Partial;
    }
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
    if matches!(
        &update,
        v2::SessionUpdate::StateUpdate(
            v2::StateUpdate::Running(_) | v2::StateUpdate::RequiresAction(_)
        )
    ) && let Some(prompt) = state
        .active_turn
        .as_ref()
        .and_then(|key| state.prompts.get_mut(key))
        && prompt.dispatched
    {
        prompt.saw_work = true;
    }
    let correlated = state
        .active_turn
        .as_ref()
        .and_then(|key| state.prompts.get(key))
        .is_some_and(|prompt| prompt.dispatched && prompt.saw_work);
    let finished_turn = if correlated
        && let v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(idle)) = &update
        && let Some(key) = state.active_turn.take()
    {
        if let (Some(store), Some(record)) = (&state.store, &state.record) {
            let mut record = record.lock().map_err(|_| Error::internal_error())?;
            // Retain only controlled terminal evidence, never an arbitrary Other payload.
            let reason = match &idle.stop_reason {
                Some(v2::StopReason::EndTurn) => "completed",
                Some(v2::StopReason::Cancelled) => "cancelled",
                Some(v2::StopReason::Refusal) => "refused",
                Some(v2::StopReason::Error(_)) => "failed",
                _ => "terminal",
            };
            if let Err(error) = store.transition(&mut record, &key, None, Some(reason)) {
                state.recovery_required = true;
                return Err(storage_error(error));
            }
        }
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
        state.observe_state(status);
    }
    publish_locked(state, id, update.clone())?;
    if is_agent_update {
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
    if previous_history
        != (
            state.history_status.source,
            state.history_status.availability,
            state.history_status.completeness,
            state.history_status.warnings.len(),
        )
    {
        forward_locked(state, id, None)?;
    }
    if let Some(failure) = failure {
        // Publish through the normal sequenced path so incremental and start replay agree.
        forward_locked(state, id, Some(failure))?;
    }
    Ok(())
}

fn v1_stop_reason(reason: agent_client_protocol::schema::v1::StopReason) -> v2::StopReason {
    use agent_client_protocol::schema::v1::StopReason;
    match reason {
        StopReason::EndTurn => v2::StopReason::EndTurn,
        StopReason::MaxTokens => v2::StopReason::MaxTokens,
        StopReason::MaxTurnRequests => v2::StopReason::MaxTurnRequests,
        StopReason::Refusal => v2::StopReason::Refusal,
        StopReason::Cancelled => v2::StopReason::Cancelled,
        // Future SDK variants must not be silently reported as successful completion.
        _ => v2::StopReason::Other(v2::OtherStopReason::new("_v1_stop", Default::default())),
    }
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

fn complete(
    control: &Mutex<Control>,
    key: &str,
    result: Result<v2::PromptResponse, PromptFailure>,
) {
    if let Ok(mut state) = control.lock() {
        if state
            .prompts
            .get(key)
            .is_some_and(|record| record.result.is_some())
        {
            return;
        }
        let uncertain = matches!(
            &result,
            Err(PromptFailure::Failed(_)
                | PromptFailure::Storage
                | PromptFailure::Uncertain { .. })
        );
        if uncertain {
            state.recovery_required = true;
        } else if result.is_err() {
            state.rollback_foreground(key);
            if state.active_turn.as_deref() == Some(key) {
                state.active_turn = None;
            }
            state.prompt_parents.remove(key);
        }
        if let (Some(store), Some(record)) = (state.store.clone(), state.record.clone())
            && let Ok(mut record) = record.lock()
        {
            let persisted = store.transition(
                &mut record,
                key,
                Some(&prompt_journal::Outcome::recorded(&result)),
                None,
            );
            if let Err(error) = persisted {
                tracing::error!(%error, "cannot persist prompt result");
                state.recovery_required = true;
                state.finish(key, Err(PromptFailure::Storage));
                drop(record);
                recovery_changed(&mut state);
                return;
            }
        }
        state.finish(key, result);
        if uncertain {
            recovery_changed(&mut state);
        }
    }
}

fn recovery_changed(state: &mut Control) {
    let id = state.record.as_ref().and_then(|record| {
        record
            .lock()
            .ok()
            .map(|record| v2::SessionId::new(record.id.clone()))
    });
    if let Some(id) = id {
        let _ = forward_locked(state, &id, None);
    }
    state.notify();
}

/// Must run under the ownership lock before any SDK publication or native HTTP call.
fn mark_dispatch(state: &mut Control, key: &str) -> agent_client_protocol::Result<()> {
    if let (Some(store), Some(record)) = (&state.store, &state.record) {
        let mut record = record.lock().map_err(|_| Error::internal_error())?;
        if let Err(error) = store.transition(
            &mut record,
            key,
            Some(&prompt_journal::Outcome::DispatchPossible {}),
            None,
        ) {
            state.recovery_required = true;
            return Err(storage_error(error));
        }
    }
    if let Some(prompt) = state.prompts.get_mut(key) {
        prompt.dispatched = true;
    }
    Ok(())
}

fn abandon_prompts(control: &Mutex<Control>) {
    let pending = control
        .lock()
        .map(|state| {
            state
                .prompts
                .iter()
                .filter(|(_, prompt)| prompt.result.is_none())
                .map(|(key, prompt)| (key.clone(), prompt.dispatched))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    for (key, dispatched) in pending {
        // An abandoned queue entry is provably unsent; a published request is not.
        let failure = if dispatched {
            "agent disconnected before its acknowledgement".into()
        } else {
            PromptFailure::NotDispatched
        };
        complete(control, &key, Err(failure));
    }
    if let Ok(mut state) = control.lock()
        && state.active_turn.is_some()
    {
        state.recovery_required = true;
        if let (Some(store), Some(record), Some(key)) = (
            state.store.clone(),
            state.record.clone(),
            state.active_turn.clone(),
        ) && let Ok(mut record) = record.lock()
        {
            if let Err(error) = store.interrupt(&mut record, &key) {
                tracing::error!(%error, "cannot persist interrupted execution evidence");
            }
            let id = v2::SessionId::new(record.id.clone());
            drop(record);
            let _ = forward_locked(&mut state, &id, None);
        }
        state.notify();
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
    // A queued cancel/close is not yet processed by the peer. Withhold synthetic dialog
    // replies until its terminal state, so they cannot win the race and complete the turn normally.
    let (interrupting, interrupted) = watch::channel(None);
    control
        .lock()
        .expect("session state is not poisoned")
        .native_interruption = Some(interrupting.clone());
    let cleanup = control.clone();
    let route = downstream.clone();
    let connection = Client
        .v2()
        .on_receive_notification(
            {
                let control = control.clone();
                let hive_id = hive_id.clone();
                let interrupting = interrupting.clone();
                async move |notification: v2::UpdateSessionNotification,
                            _cx: V2ConnectionTo<Agent>| {
                    if route.lock().map_err(|_| Error::internal_error())?.as_ref()
                        != Some(&notification.session_id)
                    {
                        return Ok(());
                    }
                    let idle = matches!(notification.update, v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(_)));
                    forward(&control, &hive_id, notification.update)?;
                    if idle { interrupting.send_replace(None); }
                    if idle && !control.lock().map_err(|_| Error::internal_error())?.hydrating { schedule_price(control.clone(), hive_id.clone()); }
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
                let interrupting = interrupting.clone();
                async move |mut request: v2::RequestPermissionRequest,
                   responder: Responder<v2::RequestPermissionResponse>,
                   cx: V2ConnectionTo<Agent>| {
                    if downstream.lock().map_err(|_| Error::internal_error())?.as_ref() != Some(&request.session_id) {
                        return responder.respond(v2::RequestPermissionResponse::new(v2::RequestPermissionOutcome::Cancelled));
                    }
                    if control.lock().map_err(|_| Error::internal_error())?.hydrating {
                        return responder.respond(v2::RequestPermissionResponse::new(v2::RequestPermissionOutcome::Cancelled));
                    }
                    request.session_id = hive_id.clone();
                    let control = control.clone();
                    let epoch = control.lock().map_err(|_| Error::internal_error())?.interaction_epoch;
                    let cancellation = responder.cancellation();
                    let mut interrupted = interrupting.subscribe();
                    cx.spawn(async move {
                        let result = cancellation.run_until_cancelled(async {
                            let result = interactions::permission_at(control, request, epoch).await;
                            let _ = interrupted.wait_for(|pending| pending.is_none()).await;
                            Ok(result)
                        }).await;
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
                let interrupting = interrupting.clone();
                async move |mut request: v2::CreateElicitationRequest,
                    responder: Responder<v2::CreateElicitationResponse>,
                    cx: V2ConnectionTo<Agent>| {
                    if control.lock().map_err(|_| Error::internal_error())?.hydrating {
                        return responder.respond(v2::CreateElicitationResponse::new(v2::ElicitationAction::Cancel));
                    }
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
                    let mut interrupted = interrupting.subscribe();
                    cx.spawn(async move {
                        let result = cancellation.run_until_cancelled(async {
                            let result = interactions::elicitation_at(control, request, epoch).await;
                            let _ = interrupted.wait_for(|pending| pending.is_none()).await;
                            Ok(result)
                        }).await;
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
                .send_initialize(v2::InitializeRequest::new(
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
                Ok(agent_client_protocol::InitializeOutcome::Initialized(response)) if response.protocol_version == ProtocolVersion::V2 && response.capabilities.session.is_some() => {
                    tracing::info!(protocol = "v2", session = true, "agent capabilities negotiated");
                    if let Ok(mut state) = control.lock() { state.history_status.restoration = Restoration::Supported; }
                }
                Ok(agent_client_protocol::InitializeOutcome::LowerVersion(_)) => {
                    // The configured adapter stays authoritative even when the peer offers v1.
                    let _ = ready.send(Err("agent selected ACPv1 but the profile declares ACPv2; set its versions to [\"v1\"] or upgrade the agent".into()));
                    return Ok(());
                }
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
            let restoring = recovering.is_some();
            let (opened, downstream_id) = if let Some(id) = recovering {
                if let Ok(mut state) = control.lock() {
                    // Only this resume's state update can satisfy its response barrier.
                    state.current_state = None;
                    state.foreground_reservation = None;
                    state.require_history_state = true;
                }
                let id = v2::SessionId::new(id);
                // Route updates sent during session/resume, before its response is available.
                if let Ok(mut route) = downstream.lock() { *route = Some(id.clone()); }
                tracing::info!(protocol = "v2", method = "session/resume", replay = "start", "restoring agent history");
                let prepared = cx.prepare_request(v2::ResumeSessionRequest::new(id.clone(), request.cwd)
                    .additional_directories(request.additional_directories)
                    .mcp_servers(request.mcp_servers)
                    .replay_from(v2::ReplayFrom::Start(v2::ReplayFromStart::new())))
                    .map(|response| Ok(session_config::Discovery { options: Some(response.config_options), modes: None }));
                match restore_history(prepared, control.clone()).await {
                    Ok(options) => (v2::NewSessionResponse::new(hive_id.clone()).config_options(options), id),
                    Err(error) => {
                        let failure = transcript::RestoreError::downstream("v2", "session/resume", &error);
                        tracing::warn!(protocol = "v2", method = "session/resume", code = ?error.code, reason = %failure, "history restoration failed");
                        let _ = ready.send(Err(failure.to_string()));
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
            if !restoring && let Ok(mut state) = control.lock() {
                state.finish_history();
                state.selected_model = selected_model(&opened.config_options);
                state.config_options = opened.config_options.clone();
            }
            let _ = ready.send(Ok((opened, downstream_id.to_string())));
            if active.await.is_err() { return Ok(()); }
            let mut configuration = Vec::<session_config::Pending>::new();
            while let Some(command) = rx.recv().await {
                configuration.retain_mut(session_config::Pending::outstanding);
                match command {
                    Command::Configure(mut change) => {
                        change.request.session_id = downstream_id.clone();
                        let pending = {
                            let state = control.lock().map_err(|_| Error::internal_error())?;
                            if state.closing {
                                drop(state);
                                change.finish(&hive_id, Err(Error::request_cancelled()));
                                continue;
                            }
                            if state.generation != change.generation {
                                drop(state);
                                change.finish(&hive_id, Err(Error::invalid_request().data("controller was superseded; take control before changing configuration")));
                                continue;
                            }
                            // Publish under the ownership lock, establishing dispatch-before-takeover ordering.
                            let request = cx.prepare_request(change.request.clone());
                            change.dispatch(request, hive_id.clone(), |_, response| Ok(response.config_options))?
                        };
                        configuration.push(pending);
                    }
                    Command::Prompt(mut request, generation, key) => {
                        request.session_id = downstream_id.clone();
                        let prompt = {
                            let mut state = control.lock().expect("session state is not poisoned");
                            if state.closing {
                                state.rollback_foreground(&key);
                                drop(state);
                                complete(&control, &key, Err(PromptFailure::Cancelled));
                                continue;
                            }
                            if state.generation != generation {
                                state.rollback_foreground(&key);
                                drop(state);
                                complete(&control, &key, Err(PromptFailure::NotDispatched));
                                continue;
                            }
                            // Publication under the lock defines the dispatch-before-handoff order.
                            if mark_dispatch(&mut state, &key).is_err() {
                                drop(state);
                                complete(&control, &key, Err(PromptFailure::Storage));
                                continue;
                            }
                            let prepared = cx.prepare_request(request);
                            let parent = state.prompt_parents.remove(&key);
                            let prepared = if let Some(parent) = &parent { prepared.forward_cancellation_from(parent.cancellation.clone()) } else { prepared };
                            if let Some(parent) = parent { state.parents.insert(prepared.id().clone(), parent); }
                            let mapping = interactions::ParentMapping::registered(&control, prepared.id().clone());
                            (prepared.block_task(), mapping)
                        };
                        let (prompt, _mapping) = prompt;
                        tokio::pin!(prompt);
                        let mut closing = None;
                        let result = loop {
                            tokio::select! {
                                result = &mut prompt => break result,
                                incoming = rx.recv() => match incoming {
                                    Some(Command::Cancel(epoch)) if epoch == 0 || current(&control, epoch) => {
                                        begin_native_interruption(&control.lock().expect("session state is not poisoned"));
                                        let _ = cx.send_notification(v2::CancelSessionNotification::new(downstream_id.clone()));
                                        interactions::invalidate(&mut control.lock().expect("session state is not poisoned"));
                                    }
                                    Some(Command::Cancel(_)) => {}
                                    Some(Command::Configure(change)) => change.finish(&hive_id, Err(session_config::failed())),
                                    Some(Command::Prompt(_, _, key)) => { complete(&control, &key, Err(PromptFailure::NotDispatched)); }
                                    Some(Command::Close(generation, reply)) => {
                                        if generation != 0 && !current(&control, generation) {
                                            let _ = reply.send(Err("controller was superseded; resume before closing".into()));
                                            continue;
                                        }
                                        // Close must reach the peer before waiting for an insertion it may cancel.
                                        begin_native_interruption(&control.lock().expect("session state is not poisoned"));
                                        let close = cx.send_request(v2::CloseSessionRequest::new(downstream_id.clone())).block_task();
                                        // Queue close before a withdrawn dialog can be mistaken for a user dismissal.
                                        interactions::invalidate(&mut control.lock().expect("session state is not poisoned"));
                                        let result = close.await.map(|_| ())
                                            .map_err(|_| "agent close failed; check its availability".to_string());
                                        interrupting.send_replace(None);
                                        if result.is_ok() {
                                            closing = Some(reply);
                                        } else {
                                            let _ = reply.send(result);
                                        }
                                    }
                                    None => return Ok(()),
                                }
                            }
                        }.map_err(|error| if error.code == Error::request_cancelled().code {
                            PromptFailure::Cancelled
                        } else {
                            PromptFailure::from("agent prompt failed; check its availability")
                        });
                        complete(&control, &key, result);
                        if let Some(reply) = closing {
                            // A peer may acknowledge close before replying to the pending prompt.
                            let _ = reply.send(Ok(()));
                            break;
                        }
                    }
                    Command::Cancel(epoch) => {
                        if epoch == 0 || current(&control, epoch) {
                            begin_native_interruption(&control.lock().expect("session state is not poisoned"));
                            let _ = cx.send_notification(v2::CancelSessionNotification::new(downstream_id.clone()));
                            // Preserve downstream cancel-before-dialog-response ordering on the same connection.
                            interactions::invalidate(&mut control.lock().expect("session state is not poisoned"));
                        }
                    }
                    Command::Close(generation, reply) => {
                        if generation != 0 && !current(&control, generation) {
                            let _ = reply.send(Err("controller was superseded; resume before closing".into()));
                            continue;
                        }
                        begin_native_interruption(&control.lock().expect("session state is not poisoned"));
                        let close = cx
                            .send_request(v2::CloseSessionRequest::new(downstream_id.clone()))
                            .block_task();
                        interactions::invalidate(&mut control.lock().expect("session state is not poisoned"));
                        for pending in configuration.drain(..) {
                            pending.cancel().await;
                        }
                        let result = close.await
                            .map(|_| ())
                            .map_err(|_| "agent close failed; check its availability".to_string());
                        interrupting.send_replace(None);
                        let closed = result.is_ok();
                        let _ = reply.send(result);
                        if closed { break; }
                    }
                }
            }
            Ok(())
        });
    let result = tokio::select! {
        result = connection => result,
        _ = native_interruption_expired(interrupted) => {
            // Drop the connection instead of releasing a dialog response that could invent completion.
            tracing::warn!(code = "hive::interactions::interruption_timeout", "agent did not confirm cancellation; inspect its state before retrying");
            Err(Error::internal_error().data("agent cancellation timed out; inspect its state before retrying"))
        }
    };
    if let Err(error) = result {
        tracing::warn!(code = ?error.code, "downstream agent connection closed");
    }
    abandon_prompts(&cleanup);
}

fn begin_native_interruption(state: &Control) {
    // Pre-insertion questions may need a cancelled reply before the peer can acknowledge
    // the prompt. Only an explicitly running lifecycle supplies a terminal-idle barrier.
    if matches!(
        state.current_state,
        Some(v2::StateUpdate::Running(_) | v2::StateUpdate::RequiresAction(_))
    ) && let Some(interrupting) = &state.native_interruption
    {
        // Repeated cancels must not extend the lifetime of an unresponsive peer.
        interrupting.send_if_modified(|deadline| {
            if deadline.is_some() {
                return false;
            }
            *deadline = Some(tokio::time::Instant::now() + std::time::Duration::from_secs(15));
            true
        });
    }
}

async fn native_interruption_expired(
    mut interrupted: watch::Receiver<Option<tokio::time::Instant>>,
) {
    loop {
        let deadline = *interrupted.borrow_and_update();
        if let Some(deadline) = deadline {
            tokio::select! {
                // Prefer an already-observed idle over an expiring watchdog at the same instant.
                biased;
                changed = interrupted.changed() => if changed.is_err() { return; },
                _ = tokio::time::sleep_until(deadline) => return,
            }
        } else if interrupted.changed().await.is_err() {
            return;
        }
    }
}

/// ACPv1 prompt completion is a response, unlike v2's independent idle update.
fn finish_v1_prompt(
    control: &Arc<Mutex<Control>>,
    hive_id: &v2::SessionId,
    result: agent_client_protocol::Result<v1::PromptResponse>,
) -> bool {
    let completed = result.is_ok();
    let (stop, tokens) = match result {
        Ok(response) => (v1_stop_reason(response.stop_reason), response.usage),
        Err(error) => {
            tracing::warn!(code = ?error.code, "v1 prompt failed");
            // Insertion already succeeded: report late failure as an event, never a second acknowledgement.
            let failure =
                hive_protocol::AgentFailure::classify(&error.data.unwrap_or_default().to_string());
            let _ = forward(control, hive_id, failure.update("acp-v1"));
            (
                v2::StopReason::Other(v2::OtherStopReason::new("_v1_error", Default::default())),
                None,
            )
        }
    };
    let usage = tokens.map(|usage| {
        v2::Usage::new(usage.total_tokens, usage.input_tokens, usage.output_tokens)
            .thought_tokens(usage.thought_tokens)
            .cached_read_tokens(usage.cached_read_tokens)
            .cached_write_tokens(usage.cached_write_tokens)
    });
    let published = forward(
        control,
        hive_id,
        v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(
            v2::IdleStateUpdate::new().stop_reason(stop).usage(usage),
        )),
    )
    .is_ok();
    schedule_price(control.clone(), hive_id.clone());
    completed && published
}

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
    let cleanup = control.clone();
    let notifications = route.clone();
    let message_id = Arc::new(Mutex::new(None::<v2::MessageId>));
    let chunk_message_id = message_id.clone();
    // V1 history lacks message IDs; role transitions delimit historical message segments.
    let history_message = Mutex::new(None::<(bool, v2::MessageId)>);
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
                            let mut state = control.lock().map_err(|_| Error::internal_error())?;
                            let options = state.legacy_config.discover(session_config::Discovery::v1(Some(update.config_options), None)?)?;
                            session_config::publish(&mut state, &hive_id, options)?;
                        }
                        v1::SessionUpdate::CurrentModeUpdate(update) => {
                            let mut state = control.lock().map_err(|_| Error::internal_error())?;
                            let options = state.legacy_config.observe(update.current_mode_id)?;
                            session_config::publish(&mut state, &hive_id, options)?;
                        }
                        v1::SessionUpdate::UsageUpdate(update) => {
                            if !control.lock().map_err(|_| Error::internal_error())?.hydrating {
                                report_v1_cost(&control, &hive_id, &update)?;
                            }
                        }
                        v1::SessionUpdate::AgentMessageChunk(chunk) | v1::SessionUpdate::UserMessageChunk(chunk) => {
                            let user = matches!(source, v1::SessionUpdate::UserMessageChunk(_));
                            let hydrating = control.lock().map_err(|_| Error::internal_error())?.hydrating;
                            // Hive already publishes the authoritative user message for live prompts.
                            if user && !hydrating { return Ok(()); }
                            let id = if hydrating {
                                let mut history = history_message.lock().map_err(|_| Error::internal_error())?;
                                let entry = history.get_or_insert_with(|| (user, v2::MessageId::new(uuid::Uuid::new_v4().to_string())));
                                if entry.0 != user { *entry = (user, v2::MessageId::new(uuid::Uuid::new_v4().to_string())); }
                                entry.1.clone()
                            } else {
                                chunk_message_id.lock().map_err(|_| Error::internal_error())?.clone().ok_or_else(Error::internal_error)?
                            };
                            let content = serde_json::from_value(serde_json::to_value(chunk.content).map_err(Error::into_internal_error)?).map_err(Error::into_internal_error)?;
                            let chunk = v2::ContentChunk::new(content, id);
                            forward(&control, &hive_id, if user { v2::SessionUpdate::UserMessageChunk(chunk) } else { v2::SessionUpdate::AgentMessageChunk(chunk) })?;
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
                    if control.lock().map_err(|_| Error::internal_error())?.hydrating {
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
                    if control.lock().map_err(|_| Error::internal_error())?.hydrating {
                        return responder.respond(v1::CreateElicitationResponse::new(v1::ElicitationAction::Cancel));
                    }
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
                Ok(response) if response.protocol_version == ProtocolVersion::V1 => {
                    tracing::info!(protocol = "v1", load_session = response.agent_capabilities.load_session, resume = response.agent_capabilities.session_capabilities.resume.is_some(), "agent capabilities negotiated");
                    response.agent_capabilities
                }
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
            if let Ok(mut state) = control.lock() {
                state.history_status.restoration = if capabilities.load_session { Restoration::Supported } else { Restoration::Unsupported };
                if !capabilities.load_session { state.history_status.warn(WarningCode::Unsupported); }
            }
            if !request.mcp_servers.is_empty() || !request.additional_directories.is_empty() {
                let _ = ready.send(Err(
                    "v1 adapter cannot translate v2 MCP server declarations or additional workspace roots; use a v2 profile for this session".into(),
                ));
                return Ok(());
            }
            let restoring = recovering.is_some();
            let (downstream_id, options) = if let Some(id) = recovering {
                let id = v1::SessionId::new(id);
                if let Ok(mut route) = route.lock() { *route = Some(id.clone()); }
                let result = if capabilities.load_session {
                    tracing::info!(protocol = "v1", method = "session/load", replay = "start", "restoring agent history");
                    let prepared = cx.prepare_request(v1::LoadSessionRequest::new(id.clone(), request.cwd.0))
                        .map(|response| session_config::Discovery::v1(response.config_options, response.modes));
                    restore_history(prepared, control.clone()).await
                } else if capabilities.session_capabilities.resume.is_some() {
                    if let Ok(mut state) = control.lock() {
                        state.history_error = Some(transcript::RestoreError::Unsupported);
                        state.history_status.availability = Availability::Unavailable;
                        state.history_status.completeness = Completeness::Partial;
                    }
                    tracing::warn!(protocol = "v1", method = "session/resume", diagnostic_code = "hive::transcript::unsupported", "agent can resume context but cannot restore history");
                    let prepared = cx.prepare_request(v1::ResumeSessionRequest::new(id.clone(), request.cwd.0))
                        .map(|response| session_config::Discovery::v1(response.config_options, response.modes));
                    restore_history(prepared, control.clone()).await
                } else {
                    let _ = ready.send(Err(format!("{}; agent also lacks session/resume, so the saved session is orphaned", transcript::RestoreError::Unsupported)));
                    return Ok(());
                };
                let options = match result {
                    Ok(options) => options,
                    Err(error) => {
                    let method = if capabilities.load_session { "session/load" } else { "session/resume" };
                    let failure = transcript::RestoreError::downstream("v1", method, &error);
                    tracing::warn!(protocol = "v1", method, code = ?error.code, reason = %failure, "history restoration failed");
                    let _ = ready.send(Err(failure.to_string()));
                    return Ok(());
                    }
                };
                (id, options)
            } else {
                match cx.send_request(v1::NewSessionRequest::new(request.cwd.0)).block_task().await {
                    Ok(response) => {
                        let mut state = control.lock().map_err(|_| Error::internal_error())?;
                        let options = state.legacy_config.discover(session_config::Discovery::v1(response.config_options, response.modes)?)?;
                        (response.session_id, options)
                    },
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
            if !restoring && let Ok(mut state) = control.lock() {
                state.finish_history();
                state.selected_model = selected_model(&options);
                state.config_options = options.clone();
            }
            let _ = ready.send(Ok((v2::NewSessionResponse::new(hive_id.clone()).config_options(options), downstream_id.to_string())));
            if active.await.is_err() { return Ok(()); }
            let mut configuration = Vec::<session_config::Pending>::new();
            while let Some(command) = rx.recv().await {
                configuration.retain_mut(session_config::Pending::outstanding);
                match command {
                    Command::Configure(change) => {
                        let legacy = control.lock().map_err(|_| Error::internal_error())?.legacy_config.route(&change.request);
                        if legacy.is_some() {
                            let v2::SessionConfigOptionValue::Id { value } = &change.request.value else {
                                change.finish(&hive_id, Err(Error::invalid_params()));
                                continue;
                            };
                            let mode = v1::SessionModeId::new(value.to_string());
                            let pending = {
                                let state = control.lock().map_err(|_| Error::internal_error())?;
                                if state.closing {
                                    drop(state);
                                    change.finish(&hive_id, Err(Error::request_cancelled()));
                                    continue;
                                }
                                if state.generation != change.generation {
                                    drop(state);
                                    change.finish(&hive_id, Err(Error::invalid_request().data("controller was superseded; take control before changing configuration")));
                                    continue;
                                }
                                let Some(revision) = state.legacy_config.route(&change.request) else {
                                    drop(state);
                                    change.finish(&hive_id, Err(Error::invalid_request().data("agent configuration changed; inspect current choices and retry")));
                                    continue;
                                };
                                let request = cx.prepare_request(v1::SetSessionModeRequest::new(downstream_id.clone(), mode.clone()).meta(change.request.meta.clone()));
                                change.dispatch(request, hive_id.clone(), move |state, _| state.legacy_config.acknowledge(revision, mode))?
                            };
                            configuration.push(pending);
                            continue;
                        }
                        let request = session_config::to_v1(&change.request, &downstream_id)?;
                        let pending = {
                            let state = control.lock().map_err(|_| Error::internal_error())?;
                            if state.closing {
                                drop(state);
                                change.finish(&hive_id, Err(Error::request_cancelled()));
                                continue;
                            }
                            if state.generation != change.generation {
                                drop(state);
                                change.finish(&hive_id, Err(Error::invalid_request().data("controller was superseded; take control before changing configuration")));
                                continue;
                            }
                            change.dispatch(cx.prepare_request(request), hive_id.clone(), |state, response| {
                                state.legacy_config.discover(session_config::Discovery::v1(Some(response.config_options), None)?)
                            })?
                        };
                        configuration.push(pending);
                    }
                    Command::Prompt(request, generation, key) => {
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
                            if let Ok(mut state) = control.lock() { state.rollback_foreground(&key); }
                            complete(&control, &key, Err(PromptFailure::NotDispatched));
                            continue;
                        };
                        let (done, response) = oneshot::channel();
                        {
                            let mut state = control.lock().map_err(|_| Error::internal_error())?;
                            // Insertion and dispatch share the admission lock: close or takeover must win before both or after both.
                            if state.closing || state.generation != generation {
                                let failure = if state.closing { PromptFailure::Cancelled } else { PromptFailure::NotDispatched };
                                state.rollback_foreground(&key);
                                drop(state);
                                complete(&control, &key, Err(failure));
                                continue;
                            }
                            if mark_dispatch(&mut state, &key).is_err() {
                                drop(state);
                                complete(&control, &key, Err(PromptFailure::Storage));
                                continue;
                            }
                            if let Ok(mut current) = message_id.lock() { *current = Some(v2::MessageId::new(format!("v1-agent-{}", uuid::Uuid::new_v4()))); }
                            forward_locked(&mut state, &hive_id, Some(v2::SessionUpdate::UserMessage(
                                v2::UserMessage::new(id.clone()).content(request.prompt),
                            )))?;
                            forward_locked(&mut state, &hive_id, Some(v2::SessionUpdate::StateUpdate(v2::StateUpdate::Running(
                                v2::RunningStateUpdate::new(),
                            ))))?;
                            let completion_control = control.clone();
                            let completion_id = hive_id.clone();
                            // Reduce at the wire boundary so no later notification can overtake terminal idle.
                            cx.prepare_request(v1::PromptRequest::new(downstream_id.clone(), blocks))
                                .on_receiving_result(async move |result| {
                                    let _ = done.send(finish_v1_prompt(&completion_control, &completion_id, result));
                                    Ok(())
                                })?;
                        }
                        complete(&control, &key, Ok(v2::PromptResponse::new(id)));
                        tokio::pin!(response);
                        let mut closing = None;
                        // V1 prompt waits for completion, so cancellation must remain live on the other lane.
                        let completed = loop {
                            tokio::select! {
                                // Publishing idle in the ordered callback can already admit the next prompt.
                                biased;
                                result = &mut response => break result.unwrap_or(false),
                                command = rx.recv() => match command {
                                     Some(Command::Cancel(epoch)) if epoch == 0 || current(&control, epoch) => {
                                         let _ = cx.send_notification(v1::CancelNotification::new(downstream_id.clone()));
                                         interactions::invalidate(&mut control.lock().expect("session state is not poisoned"));
                                     }
                                     Some(Command::Cancel(_)) => {}
                                     Some(Command::Configure(change)) => change.finish(&hive_id, Err(session_config::failed())),
                                     Some(Command::Prompt(_, _, key)) => { complete(&control, &key, Err(PromptFailure::NotDispatched)); }
                                     Some(Command::Close(generation, reply)) => {
                                         if generation != 0 && !current(&control, generation) {
                                             let _ = reply.send(Err("controller was superseded; resume before closing".into()));
                                         } else {
                                              let _ = cx.send_notification(v1::CancelNotification::new(downstream_id.clone()));
                                              interactions::invalidate(&mut control.lock().expect("session state is not poisoned"));
                                             closing = Some(reply);
                                         }
                                     }
                                    None => break false,
                                }
                            }
                        };
                        if let Some(reply) = closing {
                            let _ = reply.send(if completed { Ok(()) } else { Err("agent failed while cancelling the turn; inspect its state before retrying close".into()) });
                            if completed { break; }
                        }
                    }
                    Command::Cancel(epoch) => {
                        if epoch == 0 || current(&control, epoch) {
                            let _ = cx.send_notification(v1::CancelNotification::new(downstream_id.clone()));
                            interactions::invalidate(&mut control.lock().expect("session state is not poisoned"));
                        }
                    }
                    Command::Close(generation, reply) => {
                        if generation != 0 && !current(&control, generation) {
                            let _ = reply.send(Err("controller was superseded; resume before closing".into()));
                            continue;
                        }
                        interactions::invalidate(&mut control.lock().expect("session state is not poisoned"));
                        for pending in configuration.drain(..) {
                            pending.cancel().await;
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
    abandon_prompts(&cleanup);
}

pub fn router(broker: Broker) -> Router {
    router_with_origin(broker, None)
}

/// Only the specified browser origin and its listener-local aliases may initiate an upgrade.
pub fn router_with_origin(broker: Broker, origin: Option<&str>) -> Router {
    let reload_origin = origin.map(str::to_owned);
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
        .merge(projects::router(broker.clone(), origin.map(str::to_owned)))
        .route(
            "/v1/config/reload",
            post({
                let broker = broker.clone();
                move |headers: axum::http::HeaderMap| {
                    let broker = broker.clone();
                    let origin = reload_origin.clone();
                    async move {
                        if !reload::allowed_origin(&headers, origin.as_deref()) {
                            return Err((axum::http::StatusCode::FORBIDDEN, Json(serde_json::json!({
                                "code": "hive::reload::origin", "message": "reload origin is not allowed; use the local CLI or listener origin",
                            }))));
                        }
                        broker.reload_config().await
                            .map(|revision| Json(serde_json::json!({"reloaded": true, "revision": revision})))
                            .map_err(reload::ReloadError::response)
                    }
                }
            }),
        )
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
            get(|| async {
                (
                    [(axum::http::header::CACHE_CONTROL, "no-cache")],
                    Html(include_str!("../../../clients/hive-web/dist/index.html")),
                )
            }),
        )
        .route(
            "/ui/app.js",
            get(|| async {
                (
                    [
                        (axum::http::header::CONTENT_TYPE, "text/javascript"),
                        // Stable bundle names must be revalidated after a daemon upgrade.
                        (axum::http::header::CACHE_CONTROL, "no-cache"),
                    ],
                    include_str!("../../../clients/hive-web/dist/app.js"),
                )
            }),
        )
        .route(
            "/ui/style.css",
            get(|| async {
                (
                    [
                        (axum::http::header::CONTENT_TYPE, "text/css"),
                        (axum::http::header::CACHE_CONTROL, "no-cache"),
                    ],
                    include_str!("../../../clients/hive-web/dist/style.css"),
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

/// Independent observers must also release ownership when a driver fails without EOF.
async fn upstream_ended(cx: &V2ConnectionTo<Client>) {
    tokio::select! {
        _ = cx.incoming_closed() => {},
        _ = cx.terminated() => {},
    }
}

/// Share attachment cleanup so creation and resume preserve the same ownership fence.
fn watch_attachment(
    cx: V2ConnectionTo<Client>,
    control: Arc<Mutex<Control>>,
    id: v2::SessionId,
    subscriber: String,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        upstream_ended(&cx).await;
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
    })
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
                    // A connection-scoped task is dropped before termination is published.
                    tokio::spawn(async move {
                        upstream_ended(&connection).await;
                        if let Ok(mut capabilities) = capabilities.lock() {
                            capabilities.remove(&client_id);
                        }
                    });
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
                                responder.respond_with_error(error.wire())
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
                            hive["running"] = serde_json::json!(matches!(summary.activity, "running" | "requires_action"));
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
                    if state.busy() || state.closing || state.configuring || state.recovery_required || state.external_activity {
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
                        // Prevent takeover and fence late dialog registration in the same admission lock.
                        state.begin_close()?;
                        if entry
                            .commands
                            .send(Command::Close(state.generation, reply))
                            .is_err()
                        {
                            state.closing = false;
                            if let Some(interrupting) = &state.native_interruption { interrupting.send_replace(None); }
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
                                interactions::settled(&entry.control).await;
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
                                broker.registry.lock().map_err(|_| Error::internal_error())?
                                    .bindings.remove(&request.session_id.to_string());
                                responder.respond(v2::CloseSessionResponse::new())
                            }
                            failure => {
                                let message = match failure {
                                    Ok(Err(message)) => message,
                                    _ => "agent disconnected during close; reopen the session to inspect its state".into(),
                                };
                                if let Ok(mut state) = entry.control.lock() {
                                    state.closing = false;
                                    state.notify();
                                }
                                responder.respond_with_error(
                                    Error::internal_error().data(message),
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
    serve_inner(addr, registry, None).await
}

/// Source-backed startup keeps reload metadata out of the public configuration schema.
pub async fn serve_configured(
    addr: SocketAddr,
    source: config::ConfigSource,
) -> std::io::Result<()> {
    let registry = source.load().map_err(std::io::Error::other)?;
    serve_inner(addr, registry, Some(source)).await
}

async fn serve_inner(
    addr: SocketAddr,
    registry: Config,
    source: Option<config::ConfigSource>,
) -> std::io::Result<()> {
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
    let broker = if let Some(source) = source {
        broker.with_config_source(source)
    } else {
        broker
    };
    // Register before serving readiness: an immediate HUP must never terminate the process.
    #[cfg(unix)]
    let mut hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
    #[cfg(unix)]
    let hangups = tokio::spawn({
        let broker = broker.clone();
        async move {
            while hangup.recv().await.is_some() {
                match broker.reload_config().await {
                    Ok(revision) => tracing::info!(revision, "Hive configuration reloaded"),
                    Err(error) => {
                        tracing::warn!(code = ?miette::Diagnostic::code(&error).map(|code| code.to_string()), %error, "Hive configuration reload rejected")
                    }
                }
            }
        }
    });
    #[cfg(unix)]
    let _hangup_guard = AbortOnDrop(Some(hangups.abort_handle()));
    // Discovery stays responsive while slow or unavailable agents are reconciled.
    let recovery = tokio::spawn({
        let broker = broker.clone();
        async move { broker.reconcile().await }
    });
    let _recovery_guard = AbortOnDrop(Some(recovery.abort_handle()));
    let (shutdown_done, shut_down) = oneshot::channel();
    let shutdown_broker = broker.clone();
    let server = axum::serve(listener, router_with_origin(broker.clone(), Some(&origin)))
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            recovery.abort();
            let _ = recovery.await;
            shutdown_broker.shutdown().await;
            let _ = shutdown_done.send(());
        })
        .into_future();
    tokio::pin!(server);
    let result = tokio::select! {
        result = &mut server => result,
        _ = shut_down => tokio::time::timeout(std::time::Duration::from_secs(8), &mut server).await
            .unwrap_or_else(|_| Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "Hive HTTP shutdown timed out; active clients may need to reconnect"))),
    };
    broker.shutdown().await;
    #[cfg(unix)]
    {
        hangups.abort();
        let _ = hangups.await;
    }
    result
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

    #[test]
    fn context_only_recovery_can_offer_new_live_content_without_hiding_the_missing_prefix() {
        let mut state = detached();
        state.history_error = Some(transcript::RestoreError::Unsupported);
        state.history_status.restoration = Restoration::Unsupported;
        state.history_status.warn(WarningCode::Unsupported);
        let id = v2::SessionId::new("context-only");
        forward_locked(
            &mut state,
            &id,
            Some(v2::SessionUpdate::AgentMessageChunk(v2::ContentChunk::new(
                "new answer".into(),
                "new",
            ))),
        )
        .unwrap();
        assert_eq!(state.history_status.availability, Availability::Available);
        assert_eq!(state.history_status.source, Source::Live);
        assert_eq!(state.history_status.completeness, Completeness::Partial);
        let status = state.history_status.clone();
        forward_locked(
            &mut state,
            &id,
            Some(v2::SessionUpdate::AgentMessageChunk(v2::ContentChunk::new(
                " continues".into(),
                "new",
            ))),
        )
        .unwrap();
        assert_eq!(state.history_status, status);
        let control = serde_json::to_value(&state.events[1].1).unwrap();
        assert_eq!(control["transcript"]["completeness"], "partial");
    }

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
    async fn closing_fences_a_dialog_that_has_not_registered_before_the_worker_runs() {
        let (interrupting, mut interrupted) = watch::channel(None);
        let mut state = detached();
        state.current_state = Some(v2::StateUpdate::Running(v2::RunningStateUpdate::new()));
        state.native_interruption = Some(interrupting.clone());
        let control = Arc::new(Mutex::new(state));
        // Construct the handler now but defer registration until close admission has won.
        let dialog = interactions::permission(
            control.clone(),
            v2::RequestPermissionRequest::new("session", "Run?", vec![]),
        );
        control.lock().unwrap().begin_close().unwrap();
        assert!(matches!(
            dialog.await.outcome,
            v2::RequestPermissionOutcome::Cancelled
        ));
        assert!(
            interrupted.borrow().is_some(),
            "The response fence must already exist without worker progress"
        );
        interrupting.send_replace(None);
        interrupted
            .wait_for(|pending| pending.is_none())
            .await
            .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn repeated_cancellation_cannot_extend_an_unresponsive_agents_deadline() {
        let (interrupting, interrupted) = watch::channel(None);
        let mut state = detached();
        state.current_state = Some(v2::StateUpdate::Running(v2::RunningStateUpdate::new()));
        state.native_interruption = Some(interrupting.clone());
        let started = tokio::time::Instant::now();
        begin_native_interruption(&state);
        let deadline = *interrupting.borrow();
        tokio::time::advance(std::time::Duration::from_secs(14)).await;
        begin_native_interruption(&state);
        assert_eq!(*interrupting.borrow(), deadline);
        native_interruption_expired(interrupted).await;
        assert_eq!(started.elapsed(), std::time::Duration::from_secs(15));
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
            transcript: transcript::Transcript::default(),
            hydrating: false,
            history_error: None,
            history_status: TranscriptStatus::default(),
            require_history_state: false,
            expected_history: false,
            current_state: None,
            foreground_reservation: None,
            title: None,
            last_activity_at: None,
            catalog,
            closing: false,
            native_interruption: None,
            prompts: HashMap::new(),
            activity: Activity::new(),
            plans: plans::Plans::default(),
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
            legacy_config: session_config::Legacy::default(),
            configuring: false,
            external_activity: false,
            native: None,
        }
    }

    #[tokio::test]
    async fn restored_requires_action_is_retained_without_publishing_historical_activity() {
        let mut state = detached();
        state.hydrating = true;
        let status = v2::StateUpdate::RequiresAction(v2::RequiresActionStateUpdate::new());
        forward_locked(
            &mut state,
            &v2::SessionId::new("session"),
            Some(v2::SessionUpdate::StateUpdate(status.clone())),
        )
        .unwrap();
        assert_eq!(state.resume_state(), Some(status));
        assert_eq!(state.activity.foreground, "requires_action");
        assert!(state.busy());
        assert_eq!(state.metadata("test")["hive"]["running"], true);
        assert!(state.events.is_empty());
        assert_eq!(state.next_seq, 1);
        assert_eq!(state.last_activity_at, None);
    }

    #[tokio::test]
    async fn live_and_restored_foreground_have_the_same_admission_without_prior_running() {
        for hydrating in [false, true] {
            let mut state = detached();
            state.hydrating = hydrating;
            assert_eq!(state.foreground(), "unknown");
            for (status, busy) in [
                ("requires_action", true),
                ("idle", false),
                ("running", true),
                ("requires_action", true),
                ("idle", false),
                ("_future", false),
            ] {
                let update = serde_json::from_value(serde_json::json!({
                    "sessionUpdate":"state_update", "state":status
                }))
                .unwrap();
                forward_locked(&mut state, &v2::SessionId::new("session"), Some(update)).unwrap();
                let foreground = if status == "_future" {
                    "unknown"
                } else {
                    status
                };
                assert_eq!(state.busy(), busy, "{hydrating}/{status}");
                assert_eq!(state.foreground(), foreground);
                assert_eq!(state.activity.foreground, foreground);
                assert_eq!(state.metadata("test")["hive"]["running"], busy);
                assert_eq!(
                    state.metadata("test")["hive"]["activitySnapshot"]["foreground"],
                    foreground
                );
            }
        }
    }

    #[tokio::test]
    async fn resume_barriers_do_not_repeat_completion_accounting_or_failure_ingestion() {
        let mut state = detached();
        let id = v2::SessionId::new("session");
        let update = serde_json::from_value(serde_json::json!({
            "sessionUpdate":"state_update", "state":"idle", "stopReason":"error",
            "error":{"code":-32603,"message":"failed"},
            "usage":{"inputTokens":12,"outputTokens":3,"totalTokens":15}
        }))
        .unwrap();
        forward_locked(&mut state, &id, Some(update)).unwrap();
        // A synchronization idle must not finish work that began after the cached completion.
        state.active_turn = Some("next-turn".into());
        let usage = serde_json::to_value(state.usage.summary()).unwrap();
        let history = serde_json::to_value(state.transcript.updates().unwrap()).unwrap();
        let activity_seq = state.activity.seq;
        let timestamp = state.last_activity_at;
        for _ in 0..2 {
            let barrier = state.resume_state().unwrap();
            let value = serde_json::to_value(&barrier).unwrap();
            assert_eq!(value["state"], "idle");
            assert!(value.get("stopReason").is_none());
            assert!(value.get("error").is_none());
            assert!(value.get("usage").is_none());
            let seq = state.next_seq;
            publish_locked(&mut state, &id, v2::SessionUpdate::StateUpdate(barrier)).unwrap();
            assert_eq!(state.next_seq, seq + 1);
        }
        assert_eq!(state.active_turn.as_deref(), Some("next-turn"));
        assert_eq!(serde_json::to_value(state.usage.summary()).unwrap(), usage);
        assert_eq!(
            serde_json::to_value(state.transcript.updates().unwrap()).unwrap(),
            history
        );
        assert_eq!(state.activity.seq, activity_seq);
        assert_eq!(state.last_activity_at, timestamp);
    }

    #[tokio::test]
    async fn only_the_matching_unobserved_prompt_can_roll_back_reserved_foreground() {
        let mut state = detached();
        let idle = v2::StateUpdate::Idle(v2::IdleStateUpdate::new());
        state.observe_state(&idle);
        state.foreground_reservation = Some("prompt".into());
        assert!(matches!(
            state.resume_state(),
            Some(v2::StateUpdate::Running(_))
        ));
        assert_eq!(
            state.metadata("test")["hive"]["activitySnapshot"]["foreground"],
            "running"
        );
        state.rollback_foreground("other");
        assert_eq!(state.foreground(), "running");
        state.rollback_foreground("prompt");
        assert_eq!(state.resume_state(), Some(idle));
        assert!(!state.busy());

        state.foreground_reservation = Some("prompt".into());
        let requires_action = v2::StateUpdate::RequiresAction(v2::RequiresActionStateUpdate::new());
        state.observe_state(&requires_action);
        state.rollback_foreground("prompt");
        state.finish("prompt", Err("late failure".into()));
        assert_eq!(state.resume_state(), Some(requires_action));
    }

    #[tokio::test]
    async fn uncertain_dispatch_and_unrecognized_states_never_reuse_cached_idle() {
        let mut state = detached();
        assert!(state.resume_state().is_none());
        state.observe_state(&v2::StateUpdate::Idle(v2::IdleStateUpdate::new()));
        state.foreground_reservation = Some("prompt".into());
        state.finish("prompt", Err("uncertain dispatch".into()));
        assert!(state.resume_state().is_none());
        assert_eq!(state.foreground(), "unknown");
        state.observe_state(&v2::StateUpdate::Idle(v2::IdleStateUpdate::new()));
        let unfamiliar =
            serde_json::from_value(serde_json::json!({"state":"_unrecognized"})).unwrap();
        state.observe_state(&unfamiliar);
        assert!(state.resume_state().is_none());
    }

    #[tokio::test]
    async fn dropping_the_gateway_driver_releases_capabilities_without_waiting_for_eof() {
        let broker = Broker::new(Config::default());
        let (ready, initialized) = oneshot::channel();
        let driver = tokio::spawn(Client.v2().connect_with(
            gateway_agent(broker.clone()),
            async move |cx| {
                cx.send_initialize(v2::InitializeRequest::new(
                    ProtocolVersion::V2,
                    v2::Implementation::new("termination-test", "1"),
                ))
                .block_task()
                .await?;
                let _ = ready.send(());
                // Keep the foreground alive so abort exercises driver drop, not local shutdown.
                std::future::pending::<agent_client_protocol::Result<()>>().await
            },
        ));
        tokio::time::timeout(std::time::Duration::from_secs(2), initialized)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(broker.client_capabilities.lock().unwrap().len(), 1);
        driver.abort();
        assert!(driver.await.unwrap_err().is_cancelled());
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !broker.client_capabilities.lock().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("termination must release initialization capabilities even without EOF");
    }

    #[tokio::test]
    async fn driver_drop_releases_only_its_attachment_and_fences_a_pilot_once() {
        async fn connected() -> (
            V2ConnectionTo<Client>,
            tokio::task::JoinHandle<agent_client_protocol::Result<()>>,
            mpsc::UnboundedReceiver<v2::UpdateSessionNotification>,
        ) {
            let (connections, mut connected) = mpsc::unbounded_channel();
            let (updates, notifications) = mpsc::unbounded_channel();
            let (ready, initialized) = oneshot::channel();
            let agent = Agent.v2().on_receive_request(
                async move |request: v2::InitializeRequest,
                            responder: Responder<v2::InitializeResponse>,
                            cx: V2ConnectionTo<Client>| {
                    connections.send(cx).map_err(Error::into_internal_error)?;
                    responder.respond(
                        v2::InitializeResponse::new(
                            request.protocol_version,
                            v2::Implementation::new("fixture", "1"),
                        )
                        .capabilities(
                            v2::AgentCapabilities::new().session(v2::SessionCapabilities::new()),
                        ),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            );
            let driver = tokio::spawn(
                Client
                    .v2()
                    .on_receive_notification(
                        async move |update: v2::UpdateSessionNotification,
                                    _: V2ConnectionTo<Agent>| {
                            updates.send(update).map_err(Error::into_internal_error)
                        },
                        agent_client_protocol::on_receive_notification!(),
                    )
                    .connect_with(agent, async move |cx| {
                        cx.send_initialize(v2::InitializeRequest::new(
                            ProtocolVersion::V2,
                            v2::Implementation::new("drop-test", "1"),
                        ))
                        .block_task()
                        .await?;
                        let _ = ready.send(());
                        std::future::pending::<agent_client_protocol::Result<()>>().await
                    }),
            );
            tokio::time::timeout(std::time::Duration::from_secs(2), initialized)
                .await
                .unwrap()
                .unwrap();
            (connected.recv().await.unwrap(), driver, notifications)
        }

        for observe in [false, true] {
            let control = Arc::new(Mutex::new(detached()));
            let id = v2::SessionId::new("retained");
            let (pilot, pilot_driver, pilot_updates) = connected().await;
            let (observer, observer_driver, observer_updates) = connected().await;
            {
                let mut state = control.lock().unwrap();
                state.pilot = Some("pilot".into());
                for (name, cx) in [("pilot", pilot.clone()), ("observer", observer.clone())] {
                    state.subscribers.insert(name.into(), cx);
                    state
                        .client_capabilities
                        .insert(name.into(), v2::ClientCapabilities::new());
                }
            }
            let (
                target,
                target_driver,
                survivor_driver,
                mut survivor_updates,
                target_name,
                survivor_name,
            ) = if observe {
                (
                    observer,
                    observer_driver,
                    pilot_driver,
                    pilot_updates,
                    "observer",
                    "pilot",
                )
            } else {
                (
                    pilot,
                    pilot_driver,
                    observer_driver,
                    observer_updates,
                    "pilot",
                    "observer",
                )
            };
            // Duplicate observers model repeated attachment; join both to prove exactly-once fencing.
            let first = watch_attachment(
                target.clone(),
                control.clone(),
                id.clone(),
                target_name.into(),
            );
            let second = watch_attachment(target, control.clone(), id.clone(), target_name.into());
            target_driver.abort();
            assert!(target_driver.await.unwrap_err().is_cancelled());
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                first.await.unwrap();
                second.await.unwrap();
            })
            .await
            .expect("both cleanup observers must finish before checking ownership");
            {
                let mut state = control.lock().unwrap();
                assert_eq!(state.generation, 1 + u64::from(!observe));
                assert_eq!(state.pilot.as_deref(), observe.then_some("pilot"));
                assert_eq!(
                    state
                        .subscribers
                        .keys()
                        .map(String::as_str)
                        .collect::<Vec<_>>(),
                    [survivor_name]
                );
                assert_eq!(
                    state
                        .client_capabilities
                        .keys()
                        .map(String::as_str)
                        .collect::<Vec<_>>(),
                    [survivor_name]
                );
                forward_locked(
                    &mut state,
                    &id,
                    Some(v2::SessionUpdate::AgentMessageChunk(v2::ContentChunk::new(
                        "still attached".into(),
                        v2::MessageId::new("survivor"),
                    ))),
                )
                .unwrap();
            }
            // A retained map entry alone is insufficient: its connection must still deliver updates.
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while let Some(update) = survivor_updates.recv().await {
                    if matches!(update.update, v2::SessionUpdate::AgentMessageChunk(_)) {
                        return;
                    }
                }
                panic!("the surviving attachment disconnected");
            })
            .await
            .unwrap();
            assert!(!survivor_driver.is_finished());
            survivor_driver.abort();
            assert!(survivor_driver.await.unwrap_err().is_cancelled());
        }
    }

    #[test]
    fn every_known_v1_stop_reason_keeps_its_v2_meaning() {
        use agent_client_protocol::schema::v1::StopReason;
        for (reason, expected) in [
            (StopReason::EndTurn, "end_turn"),
            (StopReason::MaxTokens, "max_tokens"),
            (StopReason::MaxTurnRequests, "max_turn_requests"),
            (StopReason::Refusal, "refusal"),
            (StopReason::Cancelled, "cancelled"),
        ] {
            assert_eq!(
                serde_json::to_value(v1_stop_reason(reason)).unwrap()["stopReason"],
                expected
            );
        }
    }

    #[test]
    fn native_failures_are_safe_replayable_context_without_repeated_completions() {
        let mut state = detached();
        let id = v2::SessionId::new("failure");
        let active = |kind| {
            serde_json::from_value(serde_json::json!({"sessionUpdate":"state_update","state":kind}))
                .unwrap()
        };
        let failure: v2::SessionUpdate = serde_json::from_value(serde_json::json!({
            "sessionUpdate":"state_update", "state":"idle", "stopReason":"error",
            "error":{"code":-32603,"message":"secret-message", "data":"secret-data"},
            "_meta":{"private":"secret-metadata"}
        }))
        .unwrap();
        forward_locked(&mut state, &id, Some(active("requires_action"))).unwrap();
        forward_locked(&mut state, &id, Some(failure.clone())).unwrap();
        assert!(!state.busy());
        forward_locked(&mut state, &id, Some(failure.clone())).unwrap();
        let history = serde_json::to_value(state.transcript.updates().unwrap()).unwrap();
        assert_eq!(history.as_array().unwrap().len(), 1);
        assert_eq!(history[0]["code"], "hive::agent::foreground_failed");
        assert!(!history.to_string().contains("secret-"));
        let companions = state
            .events
            .iter()
            .filter(|(_, update)| {
                matches!(update,
            v2::SessionUpdate::Other(other) if other.session_update == "_hive_error")
            })
            .count();
        assert_eq!(companions, 1);
        // A new interval with identical details is a separate failure, even across blocked states.
        forward_locked(&mut state, &id, Some(active("running"))).unwrap();
        forward_locked(&mut state, &id, Some(active("requires_action"))).unwrap();
        forward_locked(&mut state, &id, Some(failure)).unwrap();
        for _ in 0..270 {
            forward_locked(
                &mut state,
                &id,
                Some(v2::SessionUpdate::Other(v2::OtherSessionUpdate::new(
                    "_padding",
                    Default::default(),
                ))),
            )
            .unwrap();
        }
        assert_eq!(state.transcript.updates().unwrap().len(), 2);
        assert!(!state.events.iter().any(|(_, update)| matches!(update,
            v2::SessionUpdate::Other(other) if other.session_update == "_hive_error")));
        assert!(state.usage.turns.is_empty());
    }

    #[test]
    fn restored_native_failures_do_not_publish_or_acknowledge_interrupted_work() {
        let mut state = detached();
        state.hydrating = true;
        state.active_turn = Some("interrupted".into());
        let id = v2::SessionId::new("failure");
        for details in [
            serde_json::json!(null),
            serde_json::json!({"bad":"sk-must-not-leak"}),
            serde_json::json!({"code":-32603,"message":"This API key has expired. sk-must-not-leak"}),
        ] {
            state.transcript = transcript::Transcript::default();
            let idle = serde_json::from_value(serde_json::json!({"sessionUpdate":"state_update",
                "state":"idle", "stopReason":"error", "error":details}))
            .unwrap();
            forward_locked(&mut state, &id, Some(idle)).unwrap();
            let history = serde_json::to_value(state.transcript.updates().unwrap()).unwrap();
            assert_eq!(history.as_array().unwrap().len(), 1);
            assert!(!history.to_string().contains("sk-must-not-leak"));
            assert!(state.events.is_empty());
            assert_eq!(state.next_seq, 1);
            assert!(state.usage.turns.is_empty());
            assert_eq!(state.active_turn.as_deref(), Some("interrupted"));
        }
    }

    #[test]
    fn a_pre_work_idle_preserves_admission_and_the_real_turn_usage() {
        for dispatched in [false, true] {
            for active in ["running", "requires_action"] {
                let mut state = detached();
                let id = v2::SessionId::new("early-idle");
                state.foreground_reservation = Some("first".into());
                state.active_turn = Some("first".into());
                state.prompts.insert(
                    "first".into(),
                    PromptRecord {
                        fingerprint: prompt_journal::Fingerprint::content(&[]).unwrap(),
                        dispatched,
                        saw_work: false,
                        result: None,
                        waiters: Vec::new(),
                    },
                );
                let idle = v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(
                    v2::IdleStateUpdate::new(),
                ));
                // Inject the snapshot directly so dispatch ordering never depends on scheduling.
                forward_locked(&mut state, &id, Some(idle)).unwrap();
                assert_eq!(state.active_turn.as_deref(), Some("first"));
                assert_eq!(state.foreground_reservation.as_deref(), Some("first"));
                assert!(state.busy());
                assert!(!state.recovery_required);
                assert!(state.usage.turns.is_empty());
                assert!(state.events.is_empty());

                state.prompts.get_mut("first").unwrap().dispatched = true;
                let started = serde_json::from_value(serde_json::json!({
                    "sessionUpdate":"state_update", "state":active
                }))
                .unwrap();
                forward_locked(&mut state, &id, Some(started)).unwrap();
                let finished = v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(
                    v2::IdleStateUpdate::new().usage(v2::Usage::new(12, 8, 4)),
                ));
                forward_locked(&mut state, &id, Some(finished.clone())).unwrap();
                forward_locked(&mut state, &id, Some(finished)).unwrap();
                assert!(state.active_turn.is_none());
                assert!(state.foreground_reservation.is_none());
                assert!(!state.busy());
                assert!(!state.recovery_required);
                assert_eq!(state.usage.summary().turns, 1);
                assert_eq!(state.usage.summary().tokens.total_tokens, Some(12));
                let turns: Vec<_> = state
                    .events
                    .iter()
                    .filter_map(|(_, event)| match event {
                        v2::SessionUpdate::Other(other)
                            if other.session_update == "_hive_usage"
                                && !other.fields["turn"].is_null() =>
                        {
                            Some(&other.fields["turn"])
                        }
                        _ => None,
                    })
                    .collect();
                assert_eq!(turns.len(), 1);
                assert_eq!(turns[0]["id"], "first");
                assert_eq!(turns[0]["tokens"]["inputTokens"], 8);
                assert_eq!(turns[0]["tokens"]["outputTokens"], 4);
            }
        }
    }

    #[test]
    fn a_bare_idle_after_foreground_work_records_one_turn_without_tokens() {
        let mut state = detached();
        let id = v2::SessionId::new("bare-idle");
        state.foreground_reservation = Some("bare".into());
        state.active_turn = Some("bare".into());
        state.prompts.insert(
            "bare".into(),
            PromptRecord {
                fingerprint: prompt_journal::Fingerprint::content(&[]).unwrap(),
                dispatched: true,
                saw_work: false,
                result: None,
                waiters: Vec::new(),
            },
        );
        forward_locked(
            &mut state,
            &id,
            Some(v2::SessionUpdate::StateUpdate(v2::StateUpdate::Running(
                v2::RunningStateUpdate::new(),
            ))),
        )
        .unwrap();
        let idle =
            v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(v2::IdleStateUpdate::new()));
        forward_locked(&mut state, &id, Some(idle.clone())).unwrap();
        forward_locked(&mut state, &id, Some(idle)).unwrap();
        assert!(state.active_turn.is_none());
        assert!(state.foreground_reservation.is_none());
        assert!(!state.busy());
        assert!(!state.recovery_required);
        assert_eq!(state.usage.summary().turns, 1);
        assert_eq!(state.usage.summary().reported_turns, 0);
        let turns: Vec<_> = state
            .events
            .iter()
            .filter_map(|(_, event)| match event {
                v2::SessionUpdate::Other(other)
                    if other.session_update == "_hive_usage" && !other.fields["turn"].is_null() =>
                {
                    Some(&other.fields["turn"])
                }
                _ => None,
            })
            .collect();
        assert_eq!(turns.len(), 1);
        assert!(turns[0]["tokens"].is_null());
    }

    #[test]
    fn a_reported_session_cost_does_not_duplicate_a_turn_or_replace_its_tokens() {
        let mut state = detached();
        let id = v2::SessionId::new("hive-usage");
        state.active_turn = Some("one".into());
        state.prompts.insert(
            "one".into(),
            PromptRecord {
                fingerprint: prompt_journal::Fingerprint::content(&[]).unwrap(),
                dispatched: true,
                saw_work: true,
                result: None,
                waiters: Vec::new(),
            },
        );
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
    fn a_transport_failure_stays_fenced_even_when_the_agent_later_reports_idle() {
        let mut state = detached();
        state.active_turn = Some("uncertain".into());
        state.prompts.insert(
            "uncertain".into(),
            PromptRecord {
                fingerprint: prompt_journal::Fingerprint::content(&[]).unwrap(),
                dispatched: true,
                saw_work: true,
                result: None,
                waiters: Vec::new(),
            },
        );
        let state = Mutex::new(state);
        complete(
            &state,
            "uncertain",
            Err("connection lost after publication".into()),
        );
        let mut state = state.lock().unwrap();
        forward_locked(
            &mut state,
            &v2::SessionId::new("original"),
            Some(v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(
                v2::IdleStateUpdate::new(),
            ))),
        )
        .unwrap();
        assert!(state.recovery_required);
        assert_eq!(
            serde_json::to_value(state.status_update()).unwrap()["recoveryRequired"],
            true
        );
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
    fn historical_replay_does_not_repeat_accounting_or_clear_interrupted_work() {
        let mut state = detached();
        state.hydrating = true;
        state.recovery_required = true;
        state.active_turn = Some("interrupted".into());
        let ledger = serde_json::to_value(&state.usage).unwrap();
        let state = Mutex::new(state);
        let id = v2::SessionId::new("original");
        for update in [
            v2::SessionUpdate::PlanUpdate(v2::PlanUpdate::new(v2::PlanUpdateContent::items(
                "restored",
                vec![v2::PlanEntry::new(
                    "Retained step",
                    v2::PlanEntryPriority::High,
                    v2::PlanEntryStatus::Completed,
                )],
            ))),
            v2::SessionUpdate::UsageUpdate(
                v2::UsageUpdate::new(120, 200_000).cost(v2::Cost::new(1.0, "USD")),
            ),
            v2::SessionUpdate::ToolCallUpdate(
                v2::ToolCallUpdate::new("old-tool")
                    .title("Previously executed")
                    .status(v2::ToolCallStatus::Completed),
            ),
            v2::SessionUpdate::AgentMessageChunk(v2::ContentChunk::new(
                "retained answer".into(),
                v2::MessageId::new("old-answer"),
            )),
            v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(
                v2::IdleStateUpdate::new().usage(v2::Usage::new(120, 80, 40)),
            )),
        ] {
            forward(&state, &id, update).unwrap();
        }
        let old_tool =
            v1::SessionUpdate::ToolCall(v1::ToolCall::new("v1-old-tool", "Already executed"));
        let raw = v2::SessionUpdate::Other(v2::OtherSessionUpdate::new(
            "_hive_v1_tool_call",
            BTreeMap::from([(
                "tool".into(),
                serde_json::json!({"toolCallId":"v1-old-tool", "title":"Already executed"}),
            )]),
        ));
        forward_v1(&state, &id, &old_tool, raw).unwrap();
        let state = state.lock().unwrap();
        assert_eq!(serde_json::to_value(&state.usage).unwrap(), ledger);
        assert_eq!(state.active_turn.as_deref(), Some("interrupted"));
        assert!(state.recovery_required);
        assert!(state.last_activity_at.is_none());
        assert_eq!(state.next_seq, 1);
        assert!(state.events.is_empty());
        assert_eq!(
            state.plans.snapshot(0)["plans"]["restored"]["plan"]["entries"][0]["status"],
            "completed"
        );
        assert_eq!(
            state.activity.plans["restored"]["entries"][0]["status"],
            "completed"
        );
        assert_eq!(state.transcript.updates().unwrap().len(), 3);
        assert!(
            serde_json::to_value(&state.activity).unwrap()["tools"]
                .as_object()
                .unwrap()
                .is_empty()
        );
        assert!(!state.busy());
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
            .is_ok()
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
    fn wrapped_incremental_history_retains_the_complete_transcript_and_current_activity() {
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
            .is_ok()
        );
        let transcript = serde_json::to_value(state.transcript.updates().unwrap()).unwrap();
        assert_eq!(transcript[1]["content"][0]["text"], "text".repeat(260));
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
