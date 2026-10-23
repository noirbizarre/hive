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
use tokio::sync::mpsc;

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
                    Ok(history) => backlog = history,
                    Err(error) => fail!(error),
                }
            }
            stream
        }
        // v2 resumes from a durable sequence number, so history comes first and yields it.
        Flavor::V2 => {
            if !matches!(attach, Attach::New) {
                match native.history(&mut mapper).await {
                    Ok(history) => backlog = history,
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
    // Backfill reaches the replay ring but not live subscribers: the client that is adopting
    // has not yet received the new session ID, and fetches history with replayFrom = start.
    if let Ok(mut state) = control.lock() {
        let subscribers = std::mem::take(&mut state.subscribers);
        for item in backlog {
            if let Mapped::Update(update) = item {
                let _ = crate::forward_locked(&mut state, &hive_id, Some(update));
            }
        }
        state.subscribers = subscribers;
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
                Some(Stream::Dropped) => worker.connection("lost"),
                Some(Stream::Reconnected) => {
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
    }
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
            Command::Prompt(request, generation, key) => {
                if !current(&self.control, generation) {
                    complete(
                        &self.control,
                        &key,
                        Err("controller was superseded before prompt dispatch; resume with a new key".into()),
                    );
                    self.clear_running();
                } else if self.turn.is_some() {
                    complete(
                        &self.control,
                        &key,
                        Err("session already has foreground work".into()),
                    );
                } else {
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
                            complete(&self.control, &key, Err(error.wire()));
                            self.clear_running();
                        }
                    }
                }
                false
            }
            Command::Cancel(epoch) => {
                if epoch == 0 || current(&self.control, epoch) {
                    // Also aborts an external turn: cancelling is a deliberate controller action.
                    if let Err(error) = self.native.abort().await {
                        tracing::warn!(code = error.code(), "OpenCode abort failed");
                    }
                }
                false
            }
            Command::Close(generation, reply) => {
                if self.turn.is_some() {
                    let _ = reply.send(Err("cancel active work before closing".into()));
                    false
                } else if generation != 0 && !current(&self.control, generation) {
                    let _ = reply.send(Err(
                        "controller was superseded; resume before closing".into()
                    ));
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

    fn clear_running(&self) {
        if let Ok(mut state) = self.control.lock() {
            state.running = false;
            state.notify();
        }
    }

    async fn events(&mut self, mapped: Vec<Mapped>) {
        let mut check = false;
        for item in mapped {
            check |= matches!(item, Mapped::CheckStatus);
            self.apply(item);
        }
        if check {
            self.poll().await;
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
                    self.turn.is_none() && self.control.lock().is_ok_and(|state| !state.running);
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
            v2::StopReason::Other(v2::OtherStopReason::new(
                "_opencode_error",
                Default::default(),
            ))
        } else {
            v2::StopReason::EndTurn
        };
        self.turn = None;
        self.failure = None;
        self.announced = false;
        let external = self
            .control
            .lock()
            .map(|mut state| std::mem::take(&mut state.external_activity))
            .unwrap_or(false);
        self.state_update(v2::StateUpdate::Idle(
            v2::IdleStateUpdate::new().stop_reason(stop),
        ));
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
            for item in history {
                self.apply(item);
            }
        }
        self.poll().await;
        self.refresh_pending().await;
    }

    async fn poll(&mut self) {
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
            // Admitted but never started (for example rejected after the 204): end the turn
            // visibly instead of leaving the session "running" forever.
            self.failure = Some(
                "OpenCode never started the prompt; check its model and provider configuration"
                    .into(),
            );
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

#[cfg(test)]
mod tests {
    use super::same_directory;

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
