//! Shared instances use the existing SDK's request multiplexer, not a second router.

use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use agent_client_protocol::{ConnectionTo, UntypedMessage, role::mcp::Server};
use hive_core::identity::NodeId;
use miette::Diagnostic;
use serde_json::Value;
use tokio::sync::{Semaphore, oneshot, watch};
use tokio::time::Instant;

use super::{InstanceIdentity, Lifecycle, process};

#[cfg(test)]
mod tests;

/// Controlled errors intentionally omit server-supplied messages and secret-bearing inputs.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error, Diagnostic)]
pub enum RuntimeError {
    #[error(
        "unsupported MCP runtime inputs; use local managed stdio, protocol 2025-11-25 and supported client capabilities"
    )]
    #[diagnostic(code(hive::mcp::runtime_inputs))]
    Inputs,
    #[error(
        "MCP server could not start; check the resolved executable, working directory and process permissions"
    )]
    #[diagnostic(code(hive::mcp::startup))]
    Startup,
    #[error(
        "MCP initialization failed or timed out; check server protocol compatibility and initialization settings"
    )]
    #[diagnostic(code(hive::mcp::initialize))]
    Initialize,
    #[error(
        "MCP connection ended; a published operation may have executed; inspect its effects before explicitly retrying"
    )]
    #[diagnostic(code(hive::mcp::connection))]
    Connection,
    #[error(
        "MCP process cleanup could not be confirmed; inspect and stop remaining server processes before restarting the daemon"
    )]
    #[diagnostic(code(hive::mcp::cleanup))]
    Cleanup,
    #[error(
        "unsupported MCP request; check the method, arguments and negotiated server capabilities"
    )]
    #[diagnostic(code(hive::mcp::request))]
    Request,
    #[error(
        "MCP request exceeds the {max_frame_bytes}-byte frame limit; reduce its arguments before retrying"
    )]
    #[diagnostic(code(hive::mcp::request_too_large))]
    RequestTooLarge { max_frame_bytes: usize },
    #[error(
        "MCP server returned JSON-RPC error {code}; inspect the method and arguments and check for side effects before retrying"
    )]
    #[diagnostic(code(hive::mcp::remote))]
    Remote { code: i32 },
    #[error(
        "MCP request cancelled; published work may still execute; inspect its effects before retrying"
    )]
    #[diagnostic(code(hive::mcp::cancelled))]
    Cancelled,
    #[error("MCP instance is busy; wait for outstanding operations before retrying")]
    #[diagnostic(code(hive::mcp::busy))]
    Busy,
    #[error("MCP restart is cooling down; retry after {retry_after_ms} milliseconds")]
    #[diagnostic(code(hive::mcp::backoff))]
    Backoff { retry_after_ms: u64 },
    #[error("MCP binding is unavailable; bind a live, non-closing session before invoking it")]
    #[diagnostic(code(hive::mcp::binding))]
    Binding,
    #[error("MCP runtime is stopping; reopen the session after the daemon restarts")]
    #[diagnostic(code(hive::mcp::stopping))]
    Stopping,
}

/// In-process policy only; managed-server TOML configuration belongs to scope resolution.
#[derive(Clone, Debug)]
pub struct RuntimePolicy {
    pub startup_timeout: Duration,
    pub shutdown_grace: Duration,
    pub exit_grace: Duration,
    pub max_frame_bytes: usize,
    pub max_concurrent_calls: usize,
    pub restart_backoff: Duration,
    pub restart_window: Duration,
}

impl Default for RuntimePolicy {
    fn default() -> Self {
        Self {
            startup_timeout: Duration::from_secs(15),
            shutdown_grace: Duration::from_secs(5),
            exit_grace: Duration::from_secs(2),
            max_frame_bytes: 8 * 1024 * 1024,
            max_concurrent_calls: 64,
            restart_backoff: Duration::from_millis(250),
            restart_window: Duration::from_secs(60),
        }
    }
}

/// Requests cannot bypass initialization or create stateful subscriptions without an owner.
pub struct McpRequest(UntypedMessage);

impl McpRequest {
    pub fn new(method: &str, params: Value) -> Result<Self, RuntimeError> {
        if !matches!(
            method,
            "ping"
                | "tools/list"
                | "tools/call"
                | "resources/list"
                | "resources/templates/list"
                | "resources/read"
                | "prompts/list"
                | "prompts/get"
        ) || !params.is_object()
            // Task augmentation settles the RPC before server work ends, defeating operation ownership.
            || params.get("task").is_some()
        {
            return Err(RuntimeError::Request);
        }
        Ok(Self(UntypedMessage {
            method: method.into(),
            params,
        }))
    }
}

#[derive(Clone)]
pub(crate) struct Ready {
    pub connection: ConnectionTo<Server>,
    pub capabilities: Value,
    pub since: Instant,
}

#[derive(Clone)]
pub(crate) enum Status {
    Starting,
    Ready(Ready),
    Failed(RuntimeError),
    Stopped,
}

struct Generation {
    status: watch::Receiver<Status>,
    stop: watch::Sender<bool>,
    done: watch::Receiver<bool>,
    starts: VecDeque<Instant>,
    failures: u32,
    retry_at: Instant,
}

pub(crate) struct Instance {
    pub identity: InstanceIdentity,
    pub policy: RuntimePolicy,
    state: Mutex<Option<Generation>>,
    leases: AtomicUsize,
    operations: AtomicUsize,
    idle_epoch: AtomicU64,
    idle_timer: Mutex<Option<tokio::task::JoinHandle<()>>>,
    slots: Arc<Semaphore>,
    stopping: AtomicBool,
    resource_waiters: Mutex<Vec<watch::Sender<bool>>>,
}

/// Runtime clones share process ownership and shutdown admission.
#[derive(Clone)]
pub struct Runtime(Arc<Registry>);

struct Registry {
    node: NodeId,
    policy: RuntimePolicy,
    instances: Mutex<Vec<Arc<Instance>>>,
    stopping: AtomicBool,
}

impl Drop for Registry {
    fn drop(&mut self) {
        // Embedders can forget explicit shutdown; still signal every owned process.
        for instance in self
            .instances
            .get_mut()
            .expect("MCP registry lock poisoned")
        {
            instance.stopping.store(true, Ordering::Release);
            let state = instance.state.lock().expect("MCP state lock poisoned");
            instance.cancel_idle();
            if let Some(state) = state.as_ref() {
                state.stop.send_replace(true);
            }
        }
    }
}

impl Runtime {
    pub fn new(node: NodeId, policy: RuntimePolicy) -> Result<Self, RuntimeError> {
        if policy.max_concurrent_calls == 0
            || policy.max_concurrent_calls > Semaphore::MAX_PERMITS
            || policy.max_frame_bytes == 0
            || policy.restart_window.is_zero()
            || [
                policy.startup_timeout,
                policy.shutdown_grace,
                policy.exit_grace,
                policy.restart_window,
            ]
            .iter()
            .any(|duration| Instant::now().checked_add(*duration).is_none())
        {
            return Err(RuntimeError::Inputs);
        }
        Ok(Self(Arc::new(Registry {
            node,
            policy,
            instances: Mutex::new(Vec::new()),
            stopping: AtomicBool::new(false),
        })))
    }

    pub fn node(&self) -> &NodeId {
        &self.0.node
    }

    /// The owner must retain this lease for the durable session, not its attachment.
    pub fn acquire(&self, identity: InstanceIdentity) -> Result<Lease, RuntimeError> {
        process::validate(&identity, &self.0.node)?;
        let mut entries = self.0.instances.lock().expect("MCP registry lock poisoned");
        if self.0.stopping.load(Ordering::Acquire) {
            return Err(RuntimeError::Stopping);
        }
        // Preserve recent failure tombstones, but do not retain secret snapshots forever.
        entries.retain(|entry| {
            entry.leases.load(Ordering::Acquire) != 0
                || entry.operations.load(Ordering::Acquire) != 0
                || entry.recent()
        });
        let instance = entries
            .iter()
            .find(|entry| entry.identity == identity)
            .cloned()
            .unwrap_or_else(|| {
                let instance = Arc::new(Instance {
                    identity,
                    policy: self.0.policy.clone(),
                    state: Mutex::new(None),
                    leases: AtomicUsize::new(0),
                    operations: AtomicUsize::new(0),
                    idle_epoch: AtomicU64::new(0),
                    idle_timer: Mutex::new(None),
                    slots: Arc::new(Semaphore::new(self.0.policy.max_concurrent_calls)),
                    stopping: AtomicBool::new(false),
                    resource_waiters: Mutex::new(Vec::new()),
                });
                entries.push(instance.clone());
                instance
            });
        {
            // Serialize reacquisition with the final idle-stop decision.
            let _state = instance.state.lock().expect("MCP state lock poisoned");
            instance.leases.fetch_add(1, Ordering::AcqRel);
            instance.cancel_idle();
        }
        // Startup failure belongs to requests; a durable binding survives lazy restart.
        let _ = instance.connection();
        Ok(Lease { instance })
    }

    pub async fn shutdown(&self) {
        let entries = {
            let entries = self.0.instances.lock().expect("MCP registry lock poisoned");
            self.0.stopping.store(true, Ordering::Release);
            entries.clone()
        };
        for entry in &entries {
            entry.stopping.store(true, Ordering::Release);
            let _state = entry.state.lock().expect("MCP state lock poisoned");
            entry.cancel_idle();
        }
        let drain = async {
            while entries
                .iter()
                .any(|entry| entry.operations.load(Ordering::Acquire) != 0)
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        let _ = tokio::time::timeout(self.0.policy.shutdown_grace, drain).await;
        let completions: Vec<_> = entries
            .iter()
            .filter_map(|entry| {
                entry
                    .state
                    .lock()
                    .expect("MCP state lock poisoned")
                    .as_ref()
                    .map(|state| {
                        state.stop.send_replace(true);
                        state.done.clone()
                    })
            })
            .collect();
        futures::future::join_all(completions.into_iter().map(|mut done| async move {
            while !*done.borrow_and_update() {
                if done.changed().await.is_err() {
                    break;
                }
            }
        }))
        .await;
    }
}

impl Instance {
    fn recent(&self) -> bool {
        self.state
            .lock()
            .expect("MCP state lock poisoned")
            .as_ref()
            .is_some_and(|state| {
                !*state.done.borrow()
                    || matches!(
                        &*state.status.borrow(),
                        Status::Failed(RuntimeError::Cleanup)
                    )
                    || state
                        .starts
                        .back()
                        .is_some_and(|last| last.elapsed() < self.policy.restart_window)
            })
    }

    fn connection(self: &Arc<Self>) -> Result<watch::Receiver<Status>, RuntimeError> {
        let mut state = self.state.lock().expect("MCP state lock poisoned");
        if self.stopping.load(Ordering::Acquire) {
            return Err(RuntimeError::Stopping);
        }
        let now = Instant::now();
        if let Some(generation) = state.as_ref() {
            if !*generation.done.borrow() {
                return Ok(generation.status.clone());
            }
            // Never replace a process tree whose termination could not be confirmed.
            if matches!(
                &*generation.status.borrow(),
                Status::Failed(RuntimeError::Cleanup)
            ) {
                return Err(RuntimeError::Cleanup);
            }
            if now < generation.retry_at {
                return Err(RuntimeError::Backoff {
                    retry_after_ms: (generation.retry_at - now)
                        .as_millis()
                        .min(u64::MAX as u128) as u64,
                });
            }
        }
        let mut starts = state
            .as_ref()
            .map(|state| state.starts.clone())
            .unwrap_or_default();
        if let Some(remaining) =
            restart_window_remaining(&mut starts, now, self.policy.restart_window)
        {
            return Err(RuntimeError::Backoff {
                retry_after_ms: remaining.as_millis().min(u64::MAX as u128) as u64,
            });
        }
        starts.push_back(now);
        let failures = state.as_ref().map_or(0, |state| state.failures);
        let (status, receiver) = watch::channel(Status::Starting);
        let (stop, stopping) = watch::channel(false);
        let (done, finished) = watch::channel(false);
        *state = Some(Generation {
            status: receiver.clone(),
            stop,
            done: finished,
            starts,
            failures,
            retry_at: now,
        });
        let instance = self.clone();
        tokio::spawn(async move {
            let error = process::run(instance.clone(), status.clone(), stopping).await;
            let healthy = matches!(&*status.borrow(), Status::Ready(ready) if ready.since.elapsed() >= instance.policy.restart_window);
            // Publication of terminal state and completion is atomic with respect to restart.
            let mut state = instance.state.lock().expect("MCP state lock poisoned");
            if let Some(state) = state.as_mut() {
                state.failures = if error.is_none() || healthy {
                    0
                } else {
                    state.failures.saturating_add(1)
                };
                let backoff = instance
                    .policy
                    .restart_backoff
                    .saturating_mul(1u32 << state.failures.saturating_sub(1).min(16))
                    .min(Duration::from_secs(30));
                state.retry_at = if error.is_some() {
                    Instant::now() + backoff
                } else {
                    Instant::now()
                };
                if error.is_none() {
                    state.starts.clear();
                }
                status.send_replace(error.map(Status::Failed).unwrap_or(Status::Stopped));
                done.send_replace(true);
            }
            instance.settle_resources(&state);
        });
        Ok(receiver)
    }

    fn cancel_idle(&self) {
        // Call with state locked so shutdown/reacquisition cannot race a replacement timer.
        // Keep the epoch guard even after abort: a timer may already be waiting for the state lock.
        self.idle_epoch.fetch_add(1, Ordering::AcqRel);
        if let Some(timer) = self
            .idle_timer
            .lock()
            .expect("MCP idle timer lock poisoned")
            .take()
        {
            timer.abort();
        }
    }

    fn idle(self: &Arc<Self>) {
        // Always lock state before timer ownership, including on the timer's completion path.
        let _state = self.state.lock().expect("MCP state lock poisoned");
        if self.stopping.load(Ordering::Acquire)
            || self.leases.load(Ordering::Acquire) != 0
            || self.operations.load(Ordering::Acquire) != 0
        {
            return;
        }
        let seconds = match self.identity.definition.lifecycle() {
            Lifecycle::OnDemand {
                idle_timeout_seconds,
            } => *idle_timeout_seconds,
            _ => return,
        };
        self.cancel_idle();
        let epoch = self.idle_epoch.load(Ordering::Acquire);
        // A sleeping timer must not keep the instance alive after its registry is dropped.
        let instance = Arc::downgrade(self);
        let mut timer = self
            .idle_timer
            .lock()
            .expect("MCP idle timer lock poisoned");
        *timer = Some(tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(seconds)).await;
            let Some(instance) = instance.upgrade() else {
                return;
            };
            let state = instance.state.lock().expect("MCP state lock poisoned");
            if instance.idle_epoch.load(Ordering::Acquire) != epoch {
                return;
            }
            // Detach our own completing handle rather than aborting ourselves while holding state.
            instance
                .idle_timer
                .lock()
                .expect("MCP idle timer lock poisoned")
                .take();
            if !instance.stopping.load(Ordering::Acquire)
                && instance.leases.load(Ordering::Acquire) == 0
                && instance.operations.load(Ordering::Acquire) == 0
                && let Some(state) = state.as_ref()
            {
                state.stop.send_replace(true);
            }
        }));
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        if let Some(timer) = self
            .idle_timer
            .get_mut()
            .expect("MCP idle timer lock poisoned")
            .take()
        {
            timer.abort();
        }
    }
}

fn restart_window_remaining(
    starts: &mut VecDeque<Instant>,
    now: Instant,
    window: Duration,
) -> Option<Duration> {
    // Pruning and delay calculation must share one instant, even at the exact window boundary.
    while starts
        .front()
        .is_some_and(|start| now.saturating_duration_since(*start) >= window)
    {
        starts.pop_front();
    }
    (starts.len() >= 3).then(|| window.saturating_sub(now.saturating_duration_since(starts[0])))
}

fn validate_outbound_request(
    request: &UntypedMessage,
    id: &agent_client_protocol::schema::v1::RequestId,
    max_frame_bytes: usize,
) -> Result<(), RuntimeError> {
    // Include the reserved SDK ID and JSON escaping before block_task publishes.
    // Keep the sink's independent limit check for other outbound messages.
    let frame = agent_client_protocol::RawJsonRpcMessage::request(
        request.method.clone(),
        request.params.clone(),
        id.clone(),
    )
    .map_err(|_| RuntimeError::Request)?;
    let bytes = serde_json::to_vec(&frame).map_err(|_| RuntimeError::Request)?;
    if bytes.len() > max_frame_bytes {
        return Err(RuntimeError::RequestTooLarge { max_frame_bytes });
    }
    Ok(())
}

/// Session ownership; dropping a caller's call handle never drops this lease.
pub struct Lease {
    instance: Arc<Instance>,
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.instance.leases.fetch_sub(1, Ordering::AcqRel);
        self.instance.idle();
        let state = self.instance.state.lock().expect("MCP state lock poisoned");
        self.instance.settle_resources(&state);
    }
}

struct Operation(Arc<Instance>, watch::Sender<bool>);
impl Drop for Operation {
    fn drop(&mut self) {
        self.0.operations.fetch_sub(1, Ordering::AcqRel);
        self.0.idle();
        let state = self.0.state.lock().expect("MCP state lock poisoned");
        self.0.settle_resources(&state);
        self.1.send_replace(true);
    }
}

impl Lease {
    /// Register before release; a waiter must not itself keep the process non-idle.
    pub(crate) fn resource_settlement(&self) -> watch::Receiver<bool> {
        let _state = self.instance.state.lock().expect("MCP state lock poisoned");
        let (sender, receiver) = watch::channel(false);
        self.instance
            .resource_waiters
            .lock()
            .expect("MCP resource lock poisoned")
            .push(sender);
        receiver
    }
    pub fn identity(&self) -> &InstanceIdentity {
        &self.instance.identity
    }

    /// Cancellation never abandons the owned response consumer or its operation guard.
    pub fn request(&self, request: McpRequest) -> McpCall {
        let (cancel, mut cancelled) = watch::channel(false);
        let (reply, received) = oneshot::channel();
        let (settled, done) = watch::channel(false);
        let instance = self.instance.clone();
        // Bound startup waiters too, before creating a task or operation reservation.
        let permit = instance.slots.clone().try_acquire_owned();
        if permit.is_err() {
            let _ = reply.send(Err(RuntimeError::Busy));
            settled.send_replace(true);
            return McpCall {
                cancel,
                received: Some(received),
                done,
            };
        }
        {
            let _state = instance.state.lock().expect("MCP state lock poisoned");
            instance.operations.fetch_add(1, Ordering::AcqRel);
            instance.cancel_idle();
        }
        let operation = Operation(instance.clone(), settled);
        tokio::spawn(async move {
            let _operation = operation;
            let _permit = permit.expect("admission checked before spawning");
            let mut reply = Some(reply);
            let ready = async {
                let mut status = instance.connection()?;
                loop {
                    let current = status.borrow_and_update().clone();
                    match current {
                        Status::Ready(ready) => {
                            let stopping = instance
                                .state
                                .lock()
                                .expect("MCP state lock poisoned")
                                .as_ref()
                                .is_some_and(|state| *state.stop.borrow());
                            if !stopping {
                                return Ok((ready, status.clone()));
                            }
                        }
                        Status::Failed(error) => return Err(error),
                        Status::Stopped => {
                            status = instance.connection()?;
                            continue;
                        }
                        Status::Starting => {}
                    }
                    status
                        .changed()
                        .await
                        .map_err(|_| RuntimeError::Connection)?;
                }
            };
            let ready = tokio::select! { biased; _ = cancellation(&mut cancelled) => Err(RuntimeError::Cancelled), ready = ready => ready };
            let result = match ready {
                Err(error) => Err(error),
                Ok((ready, mut generation)) => {
                    let capability = request.0.method.split('/').next().unwrap_or("");
                    if capability != "ping"
                        && !ready
                            .capabilities
                            .get(capability)
                            .is_some_and(Value::is_object)
                    {
                        Err(RuntimeError::Request)
                    } else {
                        if *cancelled.borrow() {
                            Err(RuntimeError::Cancelled)
                        } else {
                            let prepared = ready.connection.prepare_request(request.0.clone());
                            if let Err(error) = validate_outbound_request(
                                &request.0,
                                prepared.id(),
                                instance.policy.max_frame_bytes,
                            ) {
                                // Dropping an unpublished preparation only rejects this caller.
                                if let Some(reply) = reply.take() {
                                    let _ = reply.send(Err(error));
                                }
                                return;
                            }
                            let cancellation = prepared.cancellation_handle();
                            let response = prepared.block_task();
                            tokio::pin!(response);
                            tokio::select! {
                                result = &mut response => {
                                    let result = result.map_err(response_error);
                                    if matches!(result, Err(RuntimeError::Connection)) {
                                        if let Some(reply) = reply.take() { let _ = reply.send(result.clone()); }
                                        generation_settled(&mut generation).await;
                                    }
                                    result
                                },
                                _ = self::cancellation(&mut cancelled) => {
                                    let _ = cancellation.cancel();
                                    if let Some(reply) = reply.take() { let _ = reply.send(Err(RuntimeError::Cancelled)); }
                                    // Retain both the slot and operation until terminal settlement.
                                    let result = response.await.map_err(response_error);
                                    if matches!(result, Err(RuntimeError::Connection)) { generation_settled(&mut generation).await; }
                                    result
                                }
                            }
                        }
                    }
                }
            };
            if let Some(reply) = reply {
                let _ = reply.send(result);
            }
        });
        McpCall {
            cancel,
            received: Some(received),
            done,
        }
    }
}

impl Instance {
    fn settle_resources(&self, state: &Option<Generation>) {
        // Call under the same fence as acquisition/restart so a later generation cannot be missed.
        if self.leases.load(Ordering::Acquire) == 0
            && self.operations.load(Ordering::Acquire) == 0
            && state.as_ref().is_none_or(|generation| {
                *generation.done.borrow()
                    && !matches!(
                        &*generation.status.borrow(),
                        Status::Failed(RuntimeError::Cleanup)
                    )
            })
        {
            for waiter in self
                .resource_waiters
                .lock()
                .expect("MCP resource lock poisoned")
                .drain(..)
            {
                waiter.send_replace(true);
            }
        }
    }
}

async fn cancellation(cancelled: &mut watch::Receiver<bool>) {
    while !*cancelled.borrow_and_update() {
        if cancelled.changed().await.is_err() {
            break;
        }
    }
}

fn response_error(error: agent_client_protocol::Error) -> RuntimeError {
    // SDK markers distinguish transport loss from a peer's ordinary internal-error response.
    if agent_client_protocol::is_connection_terminated(&error)
        || agent_client_protocol::is_incoming_transport_closed(&error)
    {
        RuntimeError::Connection
    } else {
        RuntimeError::Remote {
            code: error.code.into(),
        }
    }
}

async fn generation_settled(generation: &mut watch::Receiver<Status>) {
    loop {
        let terminal = match &*generation.borrow_and_update() {
            Status::Stopped => true,
            // Preserve resource protection when termination could not be established.
            Status::Failed(RuntimeError::Cleanup) => false,
            Status::Failed(_) => true,
            _ => false,
        };
        if terminal {
            return;
        }
        if generation.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// Dropping an unconsumed call cooperatively cancels it without dropping server accounting.
pub struct McpCall {
    pub(super) cancel: watch::Sender<bool>,
    received: Option<oneshot::Receiver<Result<Value, RuntimeError>>>,
    pub(super) done: watch::Receiver<bool>,
}
impl McpCall {
    pub fn cancel(&self) {
        self.cancel.send_replace(true);
    }
    pub async fn result(mut self) -> Result<Value, RuntimeError> {
        self.received
            .take()
            .expect("call consumed once")
            .await
            .unwrap_or(Err(RuntimeError::Connection))
    }
}
impl Drop for McpCall {
    fn drop(&mut self) {
        self.cancel();
    }
}
