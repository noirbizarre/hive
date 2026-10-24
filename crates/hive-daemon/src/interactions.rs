//! Human waits are independent of SDK dispatch and bounded by the session lifecycle.

use super::*;
use hive_protocol::interactions::{validate_elicitation, validate_permission};
use std::{future::Future, time::Duration};

const LIMIT: usize = 128;
const DEADLINE: Duration = Duration::from_secs(120);

pub(super) async fn startup<T>(
    control: &Arc<Mutex<Control>>,
    ready: impl Future<Output = T>,
) -> Result<T, ()> {
    tokio::pin!(ready);
    let mut deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    loop {
        tokio::select! {
            result = &mut ready => return Ok(result),
            _ = tick.tick() => {
                // Human decisions have their own deadline; startup's watchdog measures agent silence.
                if control.lock().is_ok_and(|state| state.pending_interactions > 0) {
                    deadline = tokio::time::Instant::now() + Duration::from_secs(15);
                } else if tokio::time::Instant::now() >= deadline { return Err(()); }
            }
        }
    }
}

#[derive(Clone)]
pub(super) struct Parent {
    pub id: v1::RequestId,
    pub connection: V2ConnectionTo<Client>,
    pub capabilities: v2::ClientCapabilities,
    pub cancellation: agent_client_protocol::RequestCancellation,
    ended: watch::Receiver<bool>,
    ended_tx: watch::Sender<bool>,
    pub prompt_key: Option<String>,
}

pub(super) struct ParentEnd(watch::Sender<bool>);
impl Drop for ParentEnd {
    fn drop(&mut self) {
        self.0.send_replace(true);
    }
}
impl Parent {
    pub fn new<T: agent_client_protocol::JsonRpcResponse>(
        responder: &Responder<T>,
        connection: V2ConnectionTo<Client>,
        capabilities: v2::ClientCapabilities,
    ) -> (Self, ParentEnd) {
        let (ended, receiver) = watch::channel(false);
        (
            Self {
                id: responder.id().clone(),
                connection,
                capabilities,
                cancellation: responder.cancellation(),
                ended: receiver,
                ended_tx: ended.clone(),
                prompt_key: None,
            },
            ParentEnd(ended),
        )
    }
    fn live(&self) -> bool {
        !*self.ended.borrow() && !self.cancellation.is_cancelled()
    }
    pub fn invalidate(&self) {
        self.ended_tx.send_replace(true);
    }
}

pub(super) struct ParentMapping {
    control: Arc<Mutex<Control>>,
    id: v1::RequestId,
}
impl ParentMapping {
    pub fn new(control: &Arc<Mutex<Control>>, id: v1::RequestId, parent: Option<Parent>) -> Self {
        if let Some(parent) = parent {
            control
                .lock()
                .expect("session state is not poisoned")
                .parents
                .insert(id.clone(), parent);
        }
        Self {
            control: control.clone(),
            id,
        }
    }
    pub fn registered(control: &Arc<Mutex<Control>>, id: v1::RequestId) -> Self {
        Self {
            control: control.clone(),
            id,
        }
    }
}
impl Drop for ParentMapping {
    fn drop(&mut self) {
        if let Ok(mut state) = self.control.lock() {
            state.parents.remove(&self.id);
        }
    }
}

pub(super) struct UrlRoute {
    wire_id: String,
    recipients: Vec<V2ConnectionTo<Client>>,
    expires: tokio::time::Instant,
    started: bool,
    completion: Option<v2::CompleteElicitationNotification>,
}

pub(super) fn announce_url(
    control: &Arc<Mutex<Control>>,
    request: &v2::CreateElicitationRequest,
) -> bool {
    let v2::ElicitationMode::Url(url) = &request.mode else {
        return true;
    };
    let mut state = control.lock().expect("session state is not poisoned");
    state
        .urls
        .retain(|_, route| route.expires > tokio::time::Instant::now());
    if state.urls.contains_key(&url.elicitation_id.to_string()) || state.urls.len() >= LIMIT {
        return false;
    }
    let wire_id = uuid::Uuid::new_v4().to_string();
    state
        .early_completions
        .retain(|_, (expires, _)| *expires > tokio::time::Instant::now());
    let completion = state
        .early_completions
        .remove(&url.elicitation_id.to_string())
        .map(|(_, mut notification)| {
            notification.elicitation_id = wire_id.clone().into();
            notification
        });
    state.urls.insert(
        url.elicitation_id.to_string(),
        UrlRoute {
            wire_id,
            recipients: Vec::new(),
            expires: tokio::time::Instant::now() + DEADLINE,
            started: false,
            completion,
        },
    );
    true
}

fn deliver_completion(route: &mut UrlRoute) {
    if let Some(notification) = &route.completion {
        for recipient in route.recipients.drain(..) {
            let _ = recipient.send_notification(notification.clone());
        }
    }
}

/// Cancelling decisions must wake waiters even when ownership itself has not changed.
pub(super) fn invalidate(state: &mut Control) {
    state.interaction_epoch += 1;
    state.urls.clear();
    state.early_completions.clear();
    for parent in state.parents.values().chain(state.prompt_parents.values()) {
        parent.invalidate();
    }
    state.parents.clear();
    state.prompt_parents.clear();
    state.changed.send_modify(|_| {});
}

struct Pending(Arc<Mutex<Control>>);

struct UrlDecision {
    control: Arc<Mutex<Control>>,
    id: Option<String>,
    accepted: bool,
}
impl Drop for UrlDecision {
    fn drop(&mut self) {
        if !self.accepted
            && let Some(id) = &self.id
            && let Ok(mut state) = self.control.lock()
        {
            state.urls.remove(id);
        }
    }
}
impl Pending {
    fn register(
        control: &Arc<Mutex<Control>>,
        epoch: u64,
    ) -> Option<(Self, u64, watch::Receiver<u64>)> {
        let mut state = control.lock().ok()?;
        if state.interaction_epoch != epoch
            || state.pending_interactions >= LIMIT
            || state.closing
            || state.activity.session == "ended"
        {
            return None;
        }
        state.pending_interactions += 1;
        Some((
            Self(control.clone()),
            state.interaction_epoch,
            state.changed.subscribe(),
        ))
    }
}
impl Drop for Pending {
    fn drop(&mut self) {
        if let Ok(mut state) = self.0.lock() {
            state.pending_interactions -= 1;
        }
    }
}

pub(super) fn permission(
    control: Arc<Mutex<Control>>,
    request: v2::RequestPermissionRequest,
) -> impl Future<Output = v2::RequestPermissionResponse> {
    let epoch = control
        .lock()
        .expect("session state is not poisoned")
        .interaction_epoch;
    permission_at(control, request, epoch)
}

pub(super) async fn permission_at(
    control: Arc<Mutex<Control>>,
    request: v2::RequestPermissionRequest,
    epoch: u64,
) -> v2::RequestPermissionResponse {
    let cancelled = || v2::RequestPermissionResponse::new(v2::RequestPermissionOutcome::Cancelled);
    let Some((_pending, epoch, mut changes)) = Pending::register(&control, epoch) else {
        return cancelled();
    };
    let work = async {
        loop {
            let (gateway, generation) = {
                let state = control.lock().expect("session state is not poisoned");
                if state.interaction_epoch != epoch {
                    return cancelled();
                }
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
                    biased;
                    _ = changes.changed() => continue,
                    response = gateway.send_request(request.clone()).block_task() => {
                        let disconnected = {
                        let state = control.lock().expect("session state is not poisoned");
                        if state.interaction_epoch != epoch { return cancelled(); }
                        if state.generation != generation { continue; }
                        match response {
                            Ok(response) => return if validate_permission(&request, &response) { response } else { cancelled() },
                            // Transport EOF can fail the RPC before the disconnect watcher rotates ownership.
                            Err(error) => error.code == v1::ErrorCode::InternalError,
                        }
                        };
                        if !disconnected || changes.changed().await.is_err() { return cancelled(); }
                    }
                }
            } else if changes.changed().await.is_err() {
                return cancelled();
            }
        }
    };
    tokio::time::timeout(DEADLINE, work)
        .await
        .unwrap_or_else(|_| cancelled())
}

fn supported(state: &Control, id: &str, request: &v2::CreateElicitationRequest) -> bool {
    let Some(capabilities) = state
        .client_capabilities
        .get(id)
        .and_then(|capabilities| capabilities.elicitation.as_ref())
    else {
        return false;
    };
    match &request.mode {
        v2::ElicitationMode::Form(_) => capabilities.form.is_some(),
        v2::ElicitationMode::Url(_) => capabilities.url.is_some(),
        _ => false,
    }
}

pub(super) fn elicitation(
    control: Arc<Mutex<Control>>,
    request: v2::CreateElicitationRequest,
) -> impl Future<Output = v2::CreateElicitationResponse> {
    let epoch = control
        .lock()
        .expect("session state is not poisoned")
        .interaction_epoch;
    elicitation_at(control, request, epoch)
}

pub(super) async fn elicitation_at(
    control: Arc<Mutex<Control>>,
    request: v2::CreateElicitationRequest,
    epoch: u64,
) -> v2::CreateElicitationResponse {
    let cancelled = || v2::CreateElicitationResponse::new(v2::ElicitationAction::Cancel);
    let Some((_pending, epoch, mut changes)) = Pending::register(&control, epoch) else {
        return cancelled();
    };
    let mut url_decision = UrlDecision {
        control: control.clone(),
        id: match &request.mode {
            v2::ElicitationMode::Url(url) => Some(url.elicitation_id.to_string()),
            _ => None,
        },
        accepted: false,
    };
    if let Some(id) = &url_decision.id {
        let mut state = control.lock().expect("session state is not poisoned");
        state
            .urls
            .retain(|_, route| route.expires > tokio::time::Instant::now());
        if state.urls.get(id).is_some_and(|route| route.started)
            || (!state.urls.contains_key(id) && state.urls.len() >= LIMIT)
        {
            // Duplicate external-flow IDs cannot replace or cancel an earlier accepted flow.
            url_decision.accepted = true;
            return cancelled();
        }
        state
            .urls
            .entry(id.clone())
            .or_insert_with(|| UrlRoute {
                wire_id: uuid::Uuid::new_v4().to_string(),
                recipients: Vec::new(),
                expires: tokio::time::Instant::now() + DEADLINE,
                started: false,
                completion: None,
            })
            .started = true;
    }
    if let v2::ElicitationScope::Request(scope) = request.scope() {
        let parent = control
            .lock()
            .ok()
            .and_then(|state| state.parents.get(&scope.request_id).cloned());
        let Some(mut parent) = parent.filter(Parent::live) else {
            tracing::debug!(
                code = "hive::interactions::unmapped_parent",
                "cancelled elicitation without a live unique parent RPC"
            );
            return cancelled();
        };
        let capabilities = parent.capabilities.elicitation.as_ref();
        let supported = match &request.mode {
            v2::ElicitationMode::Form(_) => capabilities.is_some_and(|caps| caps.form.is_some()),
            v2::ElicitationMode::Url(_) => capabilities.is_some_and(|caps| caps.url.is_some()),
            _ => false,
        };
        if !supported {
            return cancelled();
        }
        let mut forwarded = request.clone();
        match &mut forwarded.mode {
            v2::ElicitationMode::Form(form) => {
                form.scope = v2::ElicitationRequestScope::new(parent.id.clone()).into()
            }
            v2::ElicitationMode::Url(url) => {
                url.scope = v2::ElicitationRequestScope::new(parent.id.clone()).into();
                let mut state = control.lock().expect("session state is not poisoned");
                let Some(route) = state.urls.get_mut(&url.elicitation_id.to_string()) else {
                    return cancelled();
                };
                route.recipients.push(parent.connection.clone());
                url.elicitation_id = route.wire_id.clone().into();
            }
            _ => return cancelled(),
        }
        let lifecycle = async {
            loop {
                if control
                    .lock()
                    .is_ok_and(|state| state.interaction_epoch != epoch)
                {
                    return;
                }
                if changes.changed().await.is_err() {
                    return;
                }
            }
        };
        let sent = parent
            .connection
            .send_request(forwarded.clone())
            .block_task();
        if let Some(id) = &url_decision.id
            && let Ok(mut state) = control.lock()
            && let Some(route) = state.urls.get_mut(id)
        {
            deliver_completion(route);
        }
        let response = tokio::select! {
            biased;
            _ = parent.ended.changed() => cancelled(),
            _ = parent.cancellation.cancelled() => cancelled(),
            _ = lifecycle => cancelled(),
            result = tokio::time::timeout(DEADLINE, sent) => {
                result.ok().and_then(Result::ok).filter(|response| parent.live() && validate_elicitation(&forwarded, response).is_ok()).unwrap_or_else(cancelled)
            }
        };
        url_decision.accepted = matches!(response.action, v2::ElicitationAction::Accept(_));
        return response;
    }
    let work = async {
        loop {
            let (gateway, generation, forwarded) = {
                let mut state = control.lock().expect("session state is not poisoned");
                if state.interaction_epoch != epoch {
                    return cancelled();
                }
                let recipient = state.pilot.as_ref().and_then(|id| {
                    state
                        .subscribers
                        .get(id)
                        .map(|gateway| (id.clone(), gateway.clone()))
                });
                let mut forwarded = request.clone();
                if let Some((id, gateway)) = &recipient {
                    if !supported(&state, id, &request) {
                        return cancelled();
                    }
                    if let v2::ElicitationMode::Url(url) = &mut forwarded.mode {
                        state
                            .urls
                            .retain(|_, route| route.expires > tokio::time::Instant::now());
                        if state.urls.len() > LIMIT {
                            return cancelled();
                        }
                        let route = state
                            .urls
                            .entry(url.elicitation_id.to_string())
                            .or_insert_with(|| UrlRoute {
                                // Worker-local IDs may collide on a shared upstream connection.
                                wire_id: uuid::Uuid::new_v4().to_string(),
                                recipients: Vec::new(),
                                expires: tokio::time::Instant::now() + DEADLINE,
                                started: true,
                                completion: None,
                            });
                        route.recipients.push(gateway.clone());
                        url.elicitation_id = route.wire_id.clone().into();
                    }
                }
                (
                    recipient.map(|(_, gateway)| gateway),
                    state.generation,
                    forwarded,
                )
            };
            if let Some(gateway) = gateway {
                let sent = gateway.send_request(forwarded.clone()).block_task();
                if let Some(id) = &url_decision.id
                    && let Ok(mut state) = control.lock()
                    && let Some(route) = state.urls.get_mut(id)
                {
                    deliver_completion(route);
                }
                tokio::select! {
                    biased;
                    _ = changes.changed() => continue,
                    response = sent => {
                        let disconnected = {
                        let state = control.lock().expect("session state is not poisoned");
                        if state.interaction_epoch != epoch { return cancelled(); }
                        if state.generation != generation { continue; }
                        match response {
                            Ok(response) => return if validate_elicitation(&forwarded, &response).is_ok() { response } else { cancelled() },
                            Err(error) => error.code == v1::ErrorCode::InternalError,
                        }
                        };
                        if !disconnected || changes.changed().await.is_err() { return cancelled(); }
                    }
                }
            } else if changes.changed().await.is_err() {
                return cancelled();
            }
        }
    };
    let response = tokio::time::timeout(DEADLINE, work)
        .await
        .unwrap_or_else(|_| cancelled());
    url_decision.accepted = matches!(response.action, v2::ElicitationAction::Accept(_));
    response
}

pub(super) fn complete(
    control: &Arc<Mutex<Control>>,
    mut notification: v2::CompleteElicitationNotification,
) {
    if let Ok(mut state) = control.lock() {
        let id = notification.elicitation_id.to_string();
        if let Some(route) = state.urls.get_mut(&id) {
            if route.expires > tokio::time::Instant::now() && route.completion.is_none() {
                notification.elicitation_id = route.wire_id.clone().into();
                route.completion = Some(notification);
                deliver_completion(route);
            }
        } else {
            // SDK request publication and notifications can race before create dispatch.
            // Retain only bounded, expiring notices from this worker, never infer user consent.
            state
                .early_completions
                .retain(|_, (expires, _)| *expires > tokio::time::Instant::now());
            if state.early_completions.len() < LIMIT {
                state
                    .early_completions
                    .entry(id)
                    .or_insert((tokio::time::Instant::now() + DEADLINE, notification));
            }
        }
    }
}
