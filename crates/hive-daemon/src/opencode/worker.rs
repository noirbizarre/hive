//! The per-session worker for an OpenCode server, the counterpart of `run_agent`.
//!
//! It plugs into the same seam as the ACP workers: it consumes `Command`s, publishes through
//! `forward`, settles prompts with `complete`, and routes permissions through the pilot. All
//! control, fencing and replay logic therefore stays in one place and is not duplicated here.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use agent_client_protocol::schema::v2;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

use super::{
    api::{self, Flavor, Native},
    error::OpencodeError,
    http::{Client, Sse},
    map::{self, Mapped, Mapper, PermissionAsk, QuestionAsk},
    version::probe,
};
use crate::{
    AbortOnDrop, Command, Control, NativeMeta, WorkerStartup, complete, config::Profile, current,
    forward, pilot_elicitation, pilot_permission, publish_activity,
};

/// How often native state is re-read to repair missed or reordered events.
const POLL: Duration = Duration::from_secs(2);
/// A stale `idle` from the previous turn can arrive right after Hive dispatches a prompt;
/// until the new turn is seen busy, idle is ignored for this long.
const IDLE_GRACE: Duration = Duration::from_secs(10);
/// How long the event stream may stay down before the session is given up as unavailable.
const STREAM_BUDGET: Duration = Duration::from_secs(300);

/// Why the worker is attaching.
pub(crate) enum Attach {
    /// Create a native session (it is a real OpenCode session, resumable there).
    New,
    /// Attach to a session found natively. Never creates one.
    Adopt(String),
    /// Reattach after a Hive restart, honouring the API flavor pinned at first attach.
    Recover {
        id: String,
        api: Option<Flavor>,
        origin: &'static str,
    },
}

enum Stream {
    Event(Value),
    Dropped,
    Reconnected,
    Lost,
}

struct Turn {
    seen_busy: bool,
    started: Instant,
}

struct Worker {
    control: Arc<Mutex<Control>>,
    hive_id: v2::SessionId,
    native: Native,
    mapper: Mapper,
    turn: Option<Turn>,
    /// Last native state we believe in, from events or polling.
    native_busy: bool,
    failure: Option<String>,
    /// Whether subscribers were told the current turn is running. A Hive-started turn is
    /// already `running` in the control state, so nothing else would announce it.
    announced: bool,
    pending: Arc<Mutex<HashMap<String, AbortOnDrop>>>,
    closing: Option<Closing>,
    child: bool,
    plan_exits: Arc<Mutex<PlanExits>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ApprovalState {
    Waiting,
    Publishing,
    Settled,
    Cancelled,
    Retry,
}

struct Approval {
    token: uuid::Uuid,
    state: ApprovalState,
}

impl Approval {
    fn complete(&mut self, token: uuid::Uuid, state: ApprovalState) -> bool {
        // Abort is cooperative: a finishing task must preserve withdrawal and settlement.
        if self.token != token
            || !matches!(
                self.state,
                ApprovalState::Waiting | ApprovalState::Publishing
            )
        {
            return false;
        }
        self.state = state;
        true
    }
}

struct PlanExits {
    online: bool,
    // Keep terminal identities for this attachment so late replay cannot recreate consent.
    requests: HashMap<String, Approval>,
}

impl PlanExits {
    fn withdraw(&mut self, terminal: bool, pending: &mut HashMap<String, AbortOnDrop>) {
        if !terminal {
            self.online = false;
        }
        for (id, approval) in &mut self.requests {
            if approval.state == ApprovalState::Waiting
                || (terminal
                    && !matches!(
                        approval.state,
                        ApprovalState::Settled | ApprovalState::Cancelled
                    ))
            {
                approval.state = if terminal {
                    ApprovalState::Cancelled
                } else {
                    ApprovalState::Retry
                };
                pending.remove(id);
            }
        }
    }

    fn reconcile(&mut self, listed: &HashSet<String>) {
        self.requests.retain(|id, approval| {
            if !listed.contains(id) {
                approval.state = ApprovalState::Settled;
            }
            // A fresh confirmation after an uncertain attempt requires a successful native read.
            !(approval.state == ApprovalState::Retry && self.online)
        });
    }
}

struct Closing {
    reply: oneshot::Sender<Result<(), String>>,
    started: Instant,
    result: Option<Result<(), String>>,
}

pub(crate) async fn run_opencode_agent(
    provider: Profile,
    request: v2::NewSessionRequest,
    hive_id: v2::SessionId,
    mut rx: mpsc::UnboundedReceiver<Command>,
    startup: WorkerStartup,
    control: Arc<Mutex<Control>>,
    attach: Attach,
) {
    let (ready, active) = startup;
    macro_rules! fail {
        ($error:expr) => {{
            let error: OpencodeError = $error;
            tracing::warn!(code = error.code(), "OpenCode attach failed");
            let _ = ready.send(Err(error.wire()));
            return;
        }};
    }
    if !request.mcp_servers.is_empty() {
        let _ = ready.send(Err(
            "OpenCode adapter cannot translate v2 MCP server declarations; configure MCP in OpenCode itself".into(),
        ));
        return;
    }
    let client = match Client::new(&provider) {
        Ok(client) => client,
        Err(error) => fail!(error),
    };
    let capabilities = match probe(&client).await {
        Ok(capabilities) => capabilities,
        Err(error) => fail!(error),
    };
    let directory = request.cwd.0.display().to_string();
    let attached = match &attach {
        Attach::New => {
            api::Native::create(&client, Flavor::for_new(&capabilities), &directory).await
        }
        Attach::Adopt(id) => api::resolve(&client, &capabilities, id, &directory, None).await,
        Attach::Recover { id, api, .. } => {
            api::resolve(&client, &capabilities, id, &directory, *api).await
        }
    };
    let (native, session) = match attached {
        Ok(attached) => attached,
        Err(error) => fail!(error),
    };
    if !matches!(attach, Attach::New) && !same_directory(&session.directory, &directory) {
        fail!(OpencodeError::DirectoryMismatch {
            native: session.directory.clone(),
            requested: directory,
        });
    }
    let origin = match &attach {
        Attach::New => "hive",
        Attach::Adopt(_) => "adopted",
        Attach::Recover { origin, .. } => origin,
    };
    if let Ok(mut state) = control.lock() {
        state.native = Some(NativeMeta {
            id: native.id.clone(),
            origin,
            api: native.flavor.as_str(),
        });
        if matches!(attach, Attach::Adopt(_)) {
            // OpenCode's placeholder title would stop Hive from naming the session on first prompt.
            state.title = session.title.clone().filter(|title| {
                !title.starts_with("New session - ") && !title.starts_with("Child session - ")
            });
            state.last_activity_at = session.updated;
        }
        state.activity.support.insert("tasks".into(), "unsupported");
    }

    let mut mapper = Mapper::new(native.flavor, &native.id);
    let mut backlog = Vec::new();
    let mut truncated = false;
    let mut cursor = None;
    let first = match native.flavor {
        // v1 has no replay cursor: open the live stream first so nothing is lost between the
        // history read and the first event. The mapper makes the overlap harmless.
        Flavor::V1 => {
            let stream = match native.events(None).await {
                Ok(stream) => stream,
                Err(error) => fail!(error),
            };
            if !matches!(attach, Attach::New) {
                match native.history(&mut mapper).await {
                    Ok(history) => {
                        backlog = history.updates;
                        truncated = history.truncated;
                    }
                    Err(error) => fail!(error),
                }
            }
            stream
        }
        // v2 resumes from a durable sequence number, so history comes first and yields it.
        Flavor::V2 => {
            if !matches!(attach, Attach::New) {
                match native.history(&mut mapper).await {
                    Ok(history) => {
                        backlog = history.updates;
                        truncated = history.truncated;
                    }
                    Err(error) => fail!(error),
                }
                cursor = backlog.iter().rev().find_map(|item| match item {
                    Mapped::Cursor(seq) => Some(*seq),
                    _ => None,
                });
            }
            match native.events(cursor).await {
                Ok(stream) => stream,
                Err(error) => fail!(error),
            }
        }
    };
    let started_busy = native.busy().await.unwrap_or(false);
    // History is display data, not new work: importing it must not repeat accounting effects.
    if let Ok(mut state) = control.lock() {
        state.history_status.restoration = crate::Restoration::Supported;
        if !matches!(attach, Attach::New) {
            state.history_status.source = crate::Source::AgentHistory;
            state.history_status.availability = crate::Availability::Available;
        }
        if truncated {
            state.history_status.partial(crate::WarningCode::Truncated);
        }
        for item in backlog {
            if let Mapped::Update(update) = item
                && let Err(error) = state.transcript.ingest(&update)
            {
                let _ = ready.send(Err(format!("history projection failed: {error}; check the adapter protocol and restart Hive")));
                return;
            }
        }
        if state.transcript.limited() {
            state
                .history_status
                .partial(crate::WarningCode::ProjectionLimited);
        }
        state.finish_history();
    }
    let _ = ready.send(Ok((
        v2::NewSessionResponse::new(hive_id.clone()),
        native.id.clone(),
    )));
    if active.await.is_err() {
        return;
    }

    let (events_tx, mut events) = mpsc::unbounded_channel();
    let _stream = AbortOnDrop(Some(
        tokio::spawn(run_events(native.clone(), first, cursor, events_tx)).abort_handle(),
    ));
    let mut worker = Worker {
        control,
        hive_id,
        native,
        mapper,
        turn: None,
        native_busy: started_busy,
        failure: None,
        announced: false,
        pending: Arc::new(Mutex::new(HashMap::new())),
        closing: None,
        child: session.parent.is_some(),
        plan_exits: Arc::new(Mutex::new(PlanExits {
            online: true,
            requests: HashMap::new(),
        })),
    };
    if started_busy {
        // The native session was mid-turn when Hive attached: surface it as external work.
        worker.apply(Mapped::Busy);
    }
    worker.refresh_pending().await;
    let mut tick = tokio::time::interval(POLL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            command = rx.recv() => match command {
                Some(command) => if worker.command(command).await { break },
                None => break,
            },
            stream = events.recv() => match stream {
                Some(Stream::Event(event)) => {
                    let mapped = worker.mapper.event(&event);
                    worker.events(mapped).await;
                }
                Some(Stream::Dropped) => {
                    worker.withdraw_approvals(false);
                    worker.connection("lost");
                    if let Some(closing) = &mut worker.closing {
                        closing.result = Some(Err("OpenCode event stream disconnected before termination was confirmed; inspect the native session and retry close".into()));
                    }
                }
                Some(Stream::Reconnected) => {
                    worker.plan_exits.lock().expect("approval state is not poisoned").online = true;
                    worker.connection("restored");
                    worker.reconcile().await;
                }
                Some(Stream::Lost) | None => {
                    worker.connection("failed");
                    break;
                }
            },
            _ = tick.tick() => worker.poll().await,
        }
        // Finish the complete mapped event batch before releasing the upstream close response.
        if worker.settle_close() {
            break;
        }
    }
    worker.withdraw_approvals(true);
    worker
        .pending
        .lock()
        .expect("pending state is not poisoned")
        .clear();
    crate::abandon_prompts(&worker.control);
}

fn same_directory(native: &str, requested: &str) -> bool {
    // A trailing slash is the only spelling difference worth tolerating; anything else means
    // a different project, and acting there would be wrong.
    Path::new(native.trim_end_matches('/')) == Path::new(requested.trim_end_matches('/'))
}

/// Pumps the SSE stream into a channel and reconnects with backoff when it drops.
async fn run_events(
    native: Native,
    mut stream: Sse,
    mut cursor: Option<u64>,
    tx: mpsc::UnboundedSender<Stream>,
) {
    let mut down_since: Option<Instant> = None;
    let mut backoff = Duration::from_millis(200);
    loop {
        // A clean end and a broken connection both mean "reconnect".
        while let Ok(Some(frame)) = stream.next().await {
            if let Ok(event) = serde_json::from_str::<Value>(&frame.data) {
                if let Some(seq) = event["durable"]["seq"].as_u64() {
                    cursor = Some(seq);
                }
                if tx.send(Stream::Event(event)).is_err() {
                    return;
                }
            }
        }
        if tx.send(Stream::Dropped).is_err() {
            return;
        }
        down_since.get_or_insert_with(Instant::now);
        stream = loop {
            if down_since.is_some_and(|since| since.elapsed() > STREAM_BUDGET) {
                let _ = tx.send(Stream::Lost);
                return;
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(5));
            match native.events(cursor).await {
                Ok(stream) => break stream,
                // Retrying wrong credentials cannot succeed and only hammers the server.
                Err(OpencodeError::Unauthorized) => {
                    let _ = tx.send(Stream::Lost);
                    return;
                }
                Err(_) => {}
            }
        };
        down_since = None;
        backoff = Duration::from_millis(200);
        if tx.send(Stream::Reconnected).is_err() {
            return;
        }
    }
}

impl Worker {
    fn withdraw_approvals(&self, terminal: bool) {
        let mut approvals = self
            .plan_exits
            .lock()
            .expect("approval state is not poisoned");
        let mut pending = self.pending.lock().expect("pending state is not poisoned");
        approvals.withdraw(terminal, &mut pending);
    }

    fn state_update(&self, update: v2::StateUpdate) {
        let _ = forward(
            &self.control,
            &self.hive_id,
            v2::SessionUpdate::StateUpdate(update),
        );
    }

    fn activity(&self, event: Value) {
        if let Ok(mut state) = self.control.lock() {
            let _ = publish_activity(&mut state, &self.hive_id, event);
        }
    }

    fn connection(&self, status: &str) {
        self.activity(json!({"kind": "connection", "status": status, "source": "opencode"}));
    }

    /// Returns `true` when the worker should stop.
    async fn command(&mut self, command: Command) -> bool {
        match command {
            Command::Configure(change) => {
                change.finish(
                    &self.hive_id,
                    Err(agent_client_protocol::Error::invalid_request()
                        .data("this backend does not expose ACP model configuration")),
                );
                false
            }
            Command::Prompt(request, generation, key) => {
                if self.control.lock().is_ok_and(|state| state.closing) {
                    if let Ok(mut state) = self.control.lock() {
                        state.rollback_foreground(&key);
                    }
                    complete(&self.control, &key, Err(crate::PromptFailure::Cancelled));
                } else if !current(&self.control, generation) {
                    if let Ok(mut state) = self.control.lock() {
                        state.rollback_foreground(&key);
                    }
                    complete(
                        &self.control,
                        &key,
                        Err(crate::PromptFailure::NotDispatched),
                    );
                } else if self.turn.is_some() {
                    complete(
                        &self.control,
                        &key,
                        Err(crate::PromptFailure::NotDispatched),
                    );
                } else {
                    let marked = match self.control.lock() {
                        Ok(mut state) => {
                            // Check ownership again at the durable publication boundary.
                            if state.generation != generation || state.closing {
                                Err(crate::PromptFailure::NotDispatched)
                            } else {
                                crate::mark_dispatch(&mut state, &key)
                                    .map_err(|_| crate::PromptFailure::Storage)
                            }
                        }
                        Err(_) => Err(crate::PromptFailure::Storage),
                    };
                    if let Err(failure) = marked {
                        complete(&self.control, &key, Err(failure));
                        return false;
                    }
                    match self.native.prompt(&request.prompt).await {
                        Ok(()) => {
                            self.turn = Some(Turn {
                                seen_busy: false,
                                started: Instant::now(),
                            });
                            // As with the v1 adapter, the response acknowledges admission; the
                            // idle state update ends the turn.
                            complete(
                                &self.control,
                                &key,
                                Ok(v2::PromptResponse::new(v2::MessageId::new(format!(
                                    "hive-opencode-{}",
                                    uuid::Uuid::new_v4()
                                )))),
                            );
                        }
                        Err(error) => {
                            tracing::warn!(code = error.code(), "OpenCode prompt failed");
                            complete(&self.control, &key, Err(error.wire().into()));
                        }
                    }
                }
                false
            }
            Command::Cancel(epoch) => {
                if epoch == 0 || current(&self.control, epoch) {
                    crate::interactions::invalidate(
                        &mut self.control.lock().expect("session state is not poisoned"),
                    );
                    self.withdraw_approvals(true);
                    // Also aborts an external turn: cancelling is a deliberate controller action.
                    if let Err(error) = self.native.abort().await {
                        tracing::warn!(code = error.code(), "OpenCode abort failed");
                    }
                }
                false
            }
            Command::Close(generation, reply) => {
                if generation == 0 || current(&self.control, generation) {
                    crate::interactions::invalidate(
                        &mut self.control.lock().expect("session state is not poisoned"),
                    );
                }
                if generation != 0 && !current(&self.control, generation) {
                    let _ = reply.send(Err(
                        "controller was superseded; resume before closing".into()
                    ));
                    false
                } else if self.turn.is_some() || self.native_busy {
                    self.withdraw_approvals(true);
                    match self.native.abort().await {
                        Ok(()) => {
                            // HTTP acceptance is not an SSE barrier: keep receiving final updates through terminal idle.
                            self.closing = Some(Closing {
                                reply,
                                started: Instant::now(),
                                result: None,
                            });
                        }
                        Err(error) => {
                            let _ = reply.send(Err(error.wire()));
                        }
                    }
                    false
                } else {
                    // Closing detaches Hive. It never deletes the native session, so the
                    // conversation stays available in OpenCode (and can be adopted again).
                    let _ = reply.send(Ok(()));
                    true
                }
            }
        }
    }

    fn settle_close(&mut self) -> bool {
        if let Some(closing) = &mut self.closing {
            if closing.result.is_none() && closing.started.elapsed() >= STREAM_BUDGET {
                closing.result = Some(Err("OpenCode did not confirm termination; inspect the native session and retry close".into()));
            }
            if let Some(result) = closing.result.take() {
                let closed = result.is_ok();
                if let Some(closing) = self.closing.take() {
                    let _ = closing.reply.send(result);
                }
                return closed;
            }
        }
        false
    }

    async fn events(&mut self, mapped: Vec<Mapped>) {
        let mut check = false;
        for item in mapped {
            check |= matches!(item, Mapped::CheckStatus);
            self.apply(item);
        }
        if check {
            if self.closing.is_some() {
                // Preview step completion is not necessarily turn completion: confirm status only
                // after its stream boundary, never from an independent timer ahead of final updates.
                if self.native.flavor == Flavor::V2
                    && self.turn.as_ref().is_none_or(|turn| turn.seen_busy)
                {
                    match self.native.busy().await {
                        Ok(false) => {
                            self.native_busy = false;
                            self.finish();
                        }
                        Ok(true) => {}
                        Err(error) => {
                            if let Some(closing) = &mut self.closing {
                                closing.result = Some(Err(error.wire()));
                            }
                        }
                    }
                }
            } else {
                self.poll().await;
            }
        }
    }

    fn apply(&mut self, item: Mapped) {
        match item {
            Mapped::Update(update) => {
                let _ = forward(&self.control, &self.hive_id, update);
            }
            Mapped::Busy => {
                self.native_busy = true;
                if let Some(turn) = self.turn.as_mut() {
                    turn.seen_busy = true;
                }
                let foreign =
                    self.turn.is_none() && self.control.lock().is_ok_and(|state| !state.busy());
                if foreign {
                    // Work Hive did not start. Hive cannot lock the native client out, so it
                    // blocks its own prompts and tells every client why.
                    if let Ok(mut state) = self.control.lock() {
                        state.external_activity = true;
                    }
                    self.activity(
                        json!({"kind": "external", "active": true, "source": "opencode"}),
                    );
                }
                if (foreign || self.turn.is_some()) && !self.announced {
                    self.announced = true;
                    self.state_update(v2::StateUpdate::Running(v2::RunningStateUpdate::new()));
                }
            }
            Mapped::Idle => {
                self.native_busy = false;
                let external = self
                    .control
                    .lock()
                    .is_ok_and(|state| state.external_activity);
                match &self.turn {
                    // A late idle from the previous turn must not end the one just dispatched.
                    Some(turn) if !turn.seen_busy && turn.started.elapsed() < IDLE_GRACE => {}
                    Some(_) => self.finish(),
                    None if external => self.finish(),
                    // Idle with nothing of ours running: either stale, or a Hive prompt that
                    // is queued but not yet dispatched. Neither may flip `running` off.
                    None => {}
                }
            }
            Mapped::Failed(message) => {
                let _ = forward(
                    &self.control,
                    &self.hive_id,
                    v2::SessionUpdate::Other(v2::OtherSessionUpdate::new(
                        "_hive_opencode_error",
                        BTreeMap::from([("message".into(), json!(message))]),
                    )),
                );
                self.failure = Some(message);
            }
            Mapped::Permission(ask) => self.permission(ask),
            Mapped::Question(ask) => self.question(ask),
            Mapped::Resolved(id) => {
                if let Ok(mut approvals) = self.plan_exits.lock()
                    && let Some(approval) = approvals.requests.get_mut(&id)
                {
                    approval.state = ApprovalState::Settled;
                }
                // Answered natively first: stop asking Hive's pilot, and never reply twice.
                if let Ok(mut pending) = self.pending.lock() {
                    pending.remove(&id);
                }
            }
            Mapped::Cursor(_) | Mapped::CheckStatus => {}
        }
    }

    fn finish(&mut self) {
        let stop = if self.failure.is_some() {
            Some(v2::StopReason::Other(v2::OtherStopReason::new(
                "_opencode_error",
                Default::default(),
            )))
        } else if self.closing.is_some() {
            Some(v2::StopReason::Cancelled)
        } else {
            // Native idle is foreground evidence, not a certificate of successful execution.
            None
        };
        if let Some(closing) = &mut self.closing {
            closing.result = Some(if self.failure.is_some() {
                Err(
                    "OpenCode failed while closing; inspect the native session before retrying"
                        .into(),
                )
            } else {
                Ok(())
            });
        }
        self.turn = None;
        self.failure = None;
        self.announced = false;
        let external = self
            .control
            .lock()
            .map(|mut state| std::mem::take(&mut state.external_activity))
            .unwrap_or(false);
        let mut idle = v2::IdleStateUpdate::new();
        idle.stop_reason = stop;
        self.state_update(v2::StateUpdate::Idle(idle));
        if external {
            self.activity(json!({"kind": "external", "active": false, "source": "opencode"}));
        }
    }

    /// Repairs state after a gap: re-reads native status, pending questions and, for v1 which
    /// has no cursor, recent history.
    async fn reconcile(&mut self) {
        if self.native.flavor == Flavor::V1
            && let Ok(history) = self.native.history(&mut self.mapper).await
        {
            if history.truncated
                && let Ok(mut state) = self.control.lock()
            {
                state.history_status.partial(crate::WarningCode::Truncated);
                let _ = crate::forward_locked(&mut state, &self.hive_id, None);
            }
            for item in history.updates {
                self.apply(item);
            }
        }
        self.poll().await;
        self.refresh_pending().await;
    }

    async fn poll(&mut self) {
        // Status and SSE use different channels: polling idle cannot prove that final stream updates arrived.
        if self.closing.is_some() {
            return;
        }
        // A failed read is not evidence of anything; the event stream reports connection loss.
        let Ok(busy) = self.native.busy().await else {
            return;
        };
        if busy && !self.native_busy {
            self.apply(Mapped::Busy);
        } else if !busy && self.native_busy {
            self.apply(Mapped::Idle);
        } else if !busy
            && let Some(turn) = &self.turn
            && !turn.seen_busy
            && turn.started.elapsed() >= IDLE_GRACE
        {
            // An unseen busy interval may have completed between polls; absence is not rejection.
            if let Ok(mut state) = self.control.lock() {
                state.recovery_required = true;
                let _ = crate::forward_locked(&mut state, &self.hive_id, None);
                state.notify();
            }
            self.finish();
        }
        // v2 has no verified permission events, and on either flavor a request the pilot left
        // unanswered must be offered again; the pending map de-duplicates what is in flight.
        self.refresh_pending().await;
    }

    async fn refresh_pending(&mut self) {
        let Ok(items) = self.native.pending().await else {
            return;
        };
        let listed: HashSet<String> = items
            .iter()
            .filter_map(|item| match item {
                Mapped::Permission(ask) => Some(ask.id.clone()),
                Mapped::Question(ask) => Some(ask.id.clone()),
                _ => None,
            })
            .collect();
        // Anything no longer listed was answered somewhere; forget it so a future request
        // that reuses nothing of it starts clean.
        if let Ok(mut pending) = self.pending.lock() {
            pending.retain(|id, _| listed.contains(id));
        }
        if let Ok(mut approvals) = self.plan_exits.lock() {
            approvals.reconcile(&listed);
        }
        for item in items {
            self.apply(item);
        }
    }

    fn permission(&mut self, ask: PermissionAsk) {
        if ask.id.is_empty() || self.is_pending(&ask.id) {
            return;
        }
        let (control, hive_id, native, pending) = (
            self.control.clone(),
            self.hive_id.clone(),
            self.native.clone(),
            self.pending.clone(),
        );
        let id = ask.id.clone();
        self.track(id, async move {
            let mut request =
                v2::RequestPermissionRequest::new(hive_id, ask.title, map::permission_options());
            if let Some(description) = ask.description {
                request = request.description(description);
            }
            let response = pilot_permission(control, request).await;
            let answered = if let v2::RequestPermissionOutcome::Selected(selected) =
                response.outcome
            {
                let reply = selected.option_id.to_string();
                if matches!(reply.as_str(), "once" | "always" | "reject") {
                    match native.reply_permission(&ask.id, &reply).await {
                        Ok(()) => true,
                        Err(error) => {
                            tracing::warn!(code = error.code(), "OpenCode permission reply failed");
                            false
                        }
                    }
                } else {
                    false
                }
            } else {
                false
            };
            if !answered && let Ok(mut pending) = pending.lock() {
                // Unanswered (timeout, cancellation or a failed reply): leave the request
                // pending in OpenCode, where a native client can still answer it, and let the
                // next poll offer it to Hive again.
                pending.remove(&ask.id);
            }
        });
    }

    fn question(&mut self, ask: QuestionAsk) {
        if ask.id.is_empty() || self.is_pending(&ask.id) {
            return;
        }
        if ask.tool.is_some() {
            self.tool_question(ask);
            return;
        }
        let Some(request) = map::elicitation(&self.hive_id, &ask) else {
            tracing::warn!("OpenCode question could not be expressed as an ACP form");
            return;
        };
        let (control, native, pending) = (
            self.control.clone(),
            self.native.clone(),
            self.pending.clone(),
        );
        let id = ask.id.clone();
        self.track(id, async move {
            let response = pilot_elicitation(control, request).await;
            let result = match response.action {
                v2::ElicitationAction::Accept(accepted) => {
                    let content = accepted.content.unwrap_or_default();
                    native
                        .reply_question(&ask.id, map::answers(&ask, &content))
                        .await
                        .map(|()| true)
                }
                // Only an explicit decline is a user decision; a cancel means "nobody answered".
                v2::ElicitationAction::Decline => {
                    native.reject_question(&ask.id).await.map(|()| true)
                }
                _ => Ok(false),
            };
            if !matches!(result, Ok(true))
                && let Ok(mut pending) = pending.lock()
            {
                pending.remove(&ask.id);
            }
        });
    }

    fn tool_question(&self, ask: QuestionAsk) {
        let epoch = self
            .control
            .lock()
            .expect("session state is not poisoned")
            .interaction_epoch;
        let token = uuid::Uuid::new_v4();
        {
            let mut approvals = self
                .plan_exits
                .lock()
                .expect("approval state is not poisoned");
            // Bound replay suppression without evicting a decision that might still be replayed.
            if !approvals.online || approvals.requests.contains_key(&ask.id) {
                return;
            }
            if approvals.requests.len() >= 1024 {
                drop(approvals);
                let error = OpencodeError::PlanApproval {
                    reason: "interaction replay capacity exhausted",
                };
                let _ = forward(
                    &self.control,
                    &self.hive_id,
                    v2::SessionUpdate::Other(v2::OtherSessionUpdate::new(
                        "_hive_opencode_error",
                        BTreeMap::from([("message".into(), json!(error.wire()))]),
                    )),
                );
                return;
            }
            approvals.requests.insert(
                ask.id.clone(),
                Approval {
                    token,
                    state: ApprovalState::Waiting,
                },
            );
        }
        let (native, control, approvals, pending, hive_id, child) = (
            self.native.clone(),
            self.control.clone(),
            self.plan_exits.clone(),
            self.pending.clone(),
            self.hive_id.clone(),
            self.child,
        );
        self.track(ask.id.clone(), async move {
            let result = answer_tool_question(&native, control.clone(), approvals.clone(), &hive_id, &ask, epoch, token, child).await;
            if let Err(error) = &result {
                tracing::warn!(code = error.code(), "OpenCode question could not be confirmed; inspect the native session before retrying");
                let _ = forward(&control, &hive_id, v2::SessionUpdate::Other(v2::OtherSessionUpdate::new(
                    "_hive_opencode_error", BTreeMap::from([("message".into(), json!(error.wire()))]),
                )));
            }
            let mut ledger = approvals.lock().expect("approval state is not poisoned");
            if let Some(approval) = ledger.requests.get_mut(&ask.id)
                && approval.complete(token, result.unwrap_or(ApprovalState::Retry))
                && approval.state == ApprovalState::Retry
                && let Ok(mut pending) = pending.lock() {
                pending.remove(&ask.id);
            }
        });
    }

    fn is_pending(&self, id: &str) -> bool {
        self.pending
            .lock()
            .is_ok_and(|pending| pending.contains_key(id))
    }

    /// Spawns and registers under one lock, so a task that finishes instantly cannot remove
    /// its entry before it exists and leave a stale one behind.
    fn track(&self, id: String, task: impl Future<Output = ()> + Send + 'static) {
        if let Ok(mut pending) = self.pending.lock() {
            let handle = tokio::spawn(task).abort_handle();
            pending.insert(id, AbortOnDrop(Some(handle)));
        }
    }
}

// The tool identity is fetched independently of SSE ordering and the bounded history import.
#[allow(clippy::too_many_arguments)]
async fn answer_tool_question(
    native: &Native,
    control: Arc<Mutex<Control>>,
    approvals: Arc<Mutex<PlanExits>>,
    hive_id: &v2::SessionId,
    ask: &QuestionAsk,
    epoch: u64,
    token: uuid::Uuid,
    child: bool,
) -> Result<ApprovalState, OpencodeError> {
    if ask.session != native.id {
        return Ok(ApprovalState::Cancelled);
    }
    let message = native
        .message(&ask.tool.as_ref().expect("tool-linked question").0)
        .await?;
    let is_plan = map::plan_exit(ask, &message).map_err(|what| OpencodeError::Protocol { what })?;
    if !is_plan {
        let Some(request) = map::elicitation(hive_id, ask) else {
            return Ok(ApprovalState::Cancelled);
        };
        let response = crate::interactions::elicitation_at(control, request, epoch).await;
        match response.action {
            v2::ElicitationAction::Accept(accepted) => {
                native
                    .reply_question(
                        &ask.id,
                        map::answers(ask, &accepted.content.unwrap_or_default()),
                    )
                    .await?
            }
            v2::ElicitationAction::Decline => native.reject_question(&ask.id).await?,
            _ => return Ok(ApprovalState::Retry),
        }
        return Ok(ApprovalState::Settled);
    }
    if child {
        // Native child orchestration is unsupported: never authorize a parent through a child.
        native.reject_question(&ask.id).await?;
        return Err(OpencodeError::PlanApproval {
            reason: "child-session transition rejected",
        });
    }
    let request = map::leave_plan_permission(hive_id.clone(), ask, &message);
    let (response, receipt) =
        crate::interactions::permission_receipt(control.clone(), request, epoch).await;
    let (Some(receipt), v2::RequestPermissionOutcome::Selected(selected)) =
        (receipt, response.outcome)
    else {
        return Ok(ApprovalState::Cancelled);
    };
    let approve = match selected.option_id.to_string().as_str() {
        "approve" => true,
        "stay" => false,
        _ => return Ok(ApprovalState::Cancelled),
    };
    // Native-first resolution may race the pilot; a successful read is required before dispatch.
    if !native
        .pending()
        .await?
        .iter()
        .any(|item| matches!(item, Mapped::Question(current) if current == ask))
    {
        return Ok(ApprovalState::Settled);
    }
    {
        let state = control.lock().expect("session state is not poisoned");
        let mut ledger = approvals.lock().expect("approval state is not poisoned");
        let online = ledger.online;
        let Some(attempt) = ledger.requests.get_mut(&ask.id) else {
            return Ok(ApprovalState::Cancelled);
        };
        if state.interaction_epoch != receipt.epoch
            || state.generation != receipt.generation
            || state.closing
            || state.activity.session == "ended"
            || !online
            || attempt.token != token
            || attempt.state != ApprovalState::Waiting
        {
            return Ok(ApprovalState::Cancelled);
        }
        // Publication admission linearizes against cancellation; HTTP already dispatched cannot be retracted.
        attempt.state = ApprovalState::Publishing;
    }
    if approve {
        native
            .reply_question(&ask.id, vec![vec!["Yes".into()]])
            .await?;
    } else {
        native.reject_question(&ask.id).await?;
    }
    Ok(ApprovalState::Settled)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_finishing_cancelled_task_cannot_suppress_reconnection_or_overwrite_its_replacement() {
        let token = uuid::Uuid::new_v4();
        let mut ledger = PlanExits {
            online: true,
            requests: HashMap::from([(
                "question".into(),
                Approval {
                    token,
                    state: ApprovalState::Waiting,
                },
            )]),
        };
        // Reproduce withdrawal after the final native read but before synchronous task completion.
        ledger.withdraw(false, &mut HashMap::new());
        assert!(
            !ledger
                .requests
                .get_mut("question")
                .unwrap()
                .complete(token, ApprovalState::Cancelled)
        );
        assert!(ledger.requests["question"].state == ApprovalState::Retry);
        ledger.online = true;
        ledger.reconcile(&HashSet::from(["question".into()]));
        assert!(
            !ledger.requests.contains_key("question"),
            "the pending request can be offered again"
        );

        let mut replacement = Approval {
            token: uuid::Uuid::new_v4(),
            state: ApprovalState::Waiting,
        };
        assert!(!replacement.complete(token, ApprovalState::Settled));
        assert!(replacement.state == ApprovalState::Waiting);
    }

    #[test]
    fn a_trailing_slash_does_not_make_a_directory_different() {
        assert!(same_directory("/work/app/", "/work/app"));
        assert!(same_directory("/work/app", "/work/app"));
    }

    #[test]
    fn a_different_project_is_never_treated_as_the_same_directory() {
        assert!(!same_directory("/work/app", "/work/other"));
        assert!(!same_directory("/work/app", "/work/app/sub"));
    }
}
