//! Durable ownership is independent of the upstream socket and downstream worker.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, Weak},
};
use tokio::sync::watch;

use super::{
    InstanceIdentity,
    runtime::{Lease, McpCall, McpRequest, Runtime, RuntimeError},
};

#[derive(Default)]
pub(crate) struct Sessions {
    operations: Mutex<HashMap<String, SessionOperations>>,
    pub(crate) configured: Mutex<HashMap<String, super::routing::Bindings>>,
    pub(crate) pending: Arc<Mutex<HashMap<String, PendingTarget>>>,
    bindings: Mutex<HashMap<String, Vec<SessionMcpLease>>>,
    retired: Mutex<HashMap<String, Vec<watch::Receiver<bool>>>>,
    cleanup: Mutex<Vec<watch::Receiver<bool>>>,
    pub(crate) admission: tokio::sync::Mutex<()>,
}

struct Operation {
    cancel: watch::Sender<bool>,
    done: watch::Receiver<bool>,
}

#[derive(Default)]
struct SessionOperations {
    providers: Vec<Weak<std::sync::atomic::AtomicBool>>,
    calls: Vec<Operation>,
}

impl SessionOperations {
    fn cancel(&self) {
        for active in self.providers.iter().filter_map(Weak::upgrade) {
            active.store(false, std::sync::atomic::Ordering::Release);
        }
        for call in &self.calls {
            call.cancel.send_replace(true);
        }
    }
}

/// Both subprocess and built-in requests publish work under the same close/shutdown fence.
pub(super) fn with_authority<T>(
    registry: &Mutex<crate::reload::Registry>,
    sessions: &Mutex<HashMap<String, crate::Entry>>,
    pending: &Mutex<HashMap<String, PendingTarget>>,
    session: &str,
    expected: Option<&Arc<Mutex<crate::Control>>>,
    work: impl FnOnce(&crate::Control, &crate::reload::Registry) -> Result<T, RuntimeError>,
) -> Result<T, RuntimeError> {
    let registry = registry.lock().map_err(|_| RuntimeError::Binding)?;
    if registry.stopping {
        return Err(RuntimeError::Stopping);
    }
    let sessions = sessions.lock().map_err(|_| RuntimeError::Binding)?;
    let pending = pending.lock().map_err(|_| RuntimeError::Binding)?;
    let control = sessions
        .get(session)
        .map(|entry| &entry.control)
        .or_else(|| pending.get(session).map(|entry| &entry.control))
        .ok_or(RuntimeError::Binding)?;
    if expected.is_some_and(|expected| !Arc::ptr_eq(control, expected)) {
        return Err(RuntimeError::Binding);
    }
    let control = control.lock().map_err(|_| RuntimeError::Binding)?;
    if control.closing || control.activity.session == "ended" {
        return Err(RuntimeError::Binding);
    }
    work(&control, &registry)
}

/// Registration happens under the same control fence as lease requests, before work can publish.
impl Sessions {
    pub(crate) fn bind_provider(
        &self,
        session: &str,
    ) -> Result<Arc<std::sync::atomic::AtomicBool>, RuntimeError> {
        let mut all = self.operations.lock().map_err(|_| RuntimeError::Binding)?;
        let operations = all.entry(session.into()).or_default();
        operations
            .providers
            .retain(|provider| provider.strong_count() != 0);
        let active = Arc::new(std::sync::atomic::AtomicBool::new(true));
        operations.providers.push(Arc::downgrade(&active));
        Ok(active)
    }

    pub(crate) fn track_operation(
        &self,
        session: &str,
        active: &std::sync::atomic::AtomicBool,
    ) -> Result<(watch::Receiver<bool>, watch::Sender<bool>), RuntimeError> {
        let mut all = self.operations.lock().map_err(|_| RuntimeError::Binding)?;
        if !active.load(std::sync::atomic::Ordering::Acquire) {
            return Err(RuntimeError::Binding);
        }
        let calls = &mut all.entry(session.into()).or_default().calls;
        calls.retain(|call| !*call.done.borrow());
        if calls.len() >= 64 {
            return Err(RuntimeError::Busy);
        }
        let (cancel, cancelled) = watch::channel(false);
        let (done, completed) = watch::channel(false);
        calls.push(Operation {
            cancel,
            done: completed,
        });
        Ok((cancelled, done))
    }
}

struct Binding {
    pending: Weak<Mutex<HashMap<String, PendingTarget>>>,
    id: uuid::Uuid,
    session: String,
    sessions: Weak<Mutex<HashMap<String, crate::Entry>>>,
    admission: Weak<Mutex<crate::reload::Registry>>,
    state: Mutex<BindingState>,
}

#[derive(Clone)]
pub(crate) struct PendingTarget {
    pub control: Arc<Mutex<crate::Control>>,
    pub cwd: hive_protocol::v2::AbsolutePath,
}

#[derive(Clone, Copy)]
pub(crate) enum StartupOwnership {
    New,
    Recovery,
}

// Recovery withdraws attempt-local authority without relinquishing durable session ownership.
pub(crate) struct PendingGuard {
    pub sessions: Arc<Sessions>,
    pub id: String,
    pub promoted: bool,
    ownership: StartupOwnership,
}
impl Drop for PendingGuard {
    fn drop(&mut self) {
        let mut pending = self
            .sessions
            .pending
            .lock()
            .expect("MCP pending lock poisoned");
        pending.remove(&self.id);
        // Keep the identity fenced until synchronous revocation finishes. A later recovery
        // must not publish bindings that this guard could then cancel or release by ID.
        if !self.promoted {
            match self.ownership {
                StartupOwnership::New => {
                    self.sessions.finish(&self.id);
                }
                StartupOwnership::Recovery => self.sessions.cancel(&self.id),
            }
        }
    }
}

impl crate::Broker {
    pub(crate) fn begin_mcp_startup(
        &self,
        id: String,
        target: PendingTarget,
        ownership: StartupOwnership,
    ) -> Result<PendingGuard, RuntimeError> {
        let registry = self.registry.lock().map_err(|_| RuntimeError::Binding)?;
        if registry.stopping {
            return Err(RuntimeError::Stopping);
        }
        let sessions = self.sessions.lock().map_err(|_| RuntimeError::Binding)?;
        let mut pending = self
            .mcp_sessions
            .pending
            .lock()
            .map_err(|_| RuntimeError::Binding)?;
        // Concurrent reconciliation must not replace another worker's pending authority.
        if sessions.contains_key(&id) || pending.contains_key(&id) {
            return Err(RuntimeError::Binding);
        }
        pending.insert(id.clone(), target);
        Ok(PendingGuard {
            sessions: self.mcp_sessions.clone(),
            id,
            promoted: false,
            ownership,
        })
    }
}

struct BindingState {
    lease: Option<Lease>,
    calls: Vec<(watch::Sender<bool>, watch::Receiver<bool>)>,
    protections: Vec<crate::workspaces::mcp::Protection>,
    // Only delivered admissions own durable access; concurrent unread deliveries are separate claims.
    accepted: bool,
    pending_deliveries: usize,
}

/// Revocable access; the Broker retains ownership even when every caller drops its handle.
#[derive(Clone)]
pub struct SessionMcpLease(Arc<Binding>);

impl SessionMcpLease {
    pub(super) fn accept_delivery(&self) {
        let mut state = self.0.state.lock().expect("MCP binding lock poisoned");
        state.pending_deliveries -= 1;
        state.accepted = true;
    }

    pub fn id(&self) -> uuid::Uuid {
        self.0.id
    }

    pub(super) fn bound(&self) -> bool {
        self.0.state.lock().is_ok_and(|state| state.lease.is_some())
    }

    pub fn request(&self, request: McpRequest) -> Result<McpCall, RuntimeError> {
        let admission = self.0.admission.upgrade().ok_or(RuntimeError::Binding)?;
        let sessions = self.0.sessions.upgrade().ok_or(RuntimeError::Binding)?;
        let pending = self.0.pending.upgrade().ok_or(RuntimeError::Binding)?;
        with_authority(
            &admission,
            &sessions,
            &pending,
            &self.0.session,
            None,
            |_, _| {
                let mut state = self.0.state.lock().map_err(|_| RuntimeError::Binding)?;
                let lease = state.lease.as_ref().ok_or(RuntimeError::Binding)?;
                // Register cancellation under the close fence before the spawned request can publish.
                let call = lease.request(request);
                state.calls.retain(|(_, done)| !*done.borrow());
                state.calls.push((call.cancel.clone(), call.done.clone()));
                Ok(call)
            },
        )
    }
}

impl Sessions {
    #[cfg(test)]
    pub(crate) fn pending_deliveries(&self, session: &str) -> usize {
        self.bindings
            .lock()
            .unwrap()
            .get(session)
            .into_iter()
            .flatten()
            .map(|binding| binding.0.state.lock().unwrap().pending_deliveries)
            .sum()
    }

    pub(crate) fn bind(
        &self,
        session: String,
        sessions: &Arc<Mutex<HashMap<String, crate::Entry>>>,
        admission: &Arc<Mutex<crate::reload::Registry>>,
        runtime: &Runtime,
        identity: InstanceIdentity,
    ) -> Result<SessionMcpLease, RuntimeError> {
        self.bind_inner(session, sessions, admission, runtime, identity, None, false)
            .map(|(lease, _)| lease)
    }

    /// The bool reports reservation consumption, independently of binding creation/delivery.
    pub(crate) fn bind_protected(
        &self,
        session: String,
        sessions: &Arc<Mutex<HashMap<String, crate::Entry>>>,
        admission: &Arc<Mutex<crate::reload::Registry>>,
        runtime: &Runtime,
        identity: InstanceIdentity,
        protection: Option<crate::workspaces::mcp::Protection>,
    ) -> Result<(SessionMcpLease, bool), RuntimeError> {
        self.bind_inner(
            session, sessions, admission, runtime, identity, protection, true,
        )
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "binding publication atomically transfers protection and registers delivery ownership"
    )]
    fn bind_inner(
        &self,
        session: String,
        sessions: &Arc<Mutex<HashMap<String, crate::Entry>>>,
        admission: &Arc<Mutex<crate::reload::Registry>>,
        runtime: &Runtime,
        identity: InstanceIdentity,
        protection: Option<crate::workspaces::mcp::Protection>,
        delivery: bool,
    ) -> Result<(SessionMcpLease, bool), RuntimeError> {
        let mut all = self.bindings.lock().map_err(|_| RuntimeError::Binding)?;
        let bindings = all.entry(session.clone()).or_default();
        for binding in bindings.iter() {
            let mut state = binding.0.state.lock().map_err(|_| RuntimeError::Binding)?;
            if state
                .lease
                .as_ref()
                .is_some_and(|lease| lease.identity() == &identity)
            {
                if delivery {
                    state.pending_deliveries = state
                        .pending_deliveries
                        .checked_add(1)
                        .ok_or(RuntimeError::Busy)?;
                } else {
                    state.accepted = true;
                }
                // An equal low-level binding may be unprotected; new resources can also appear
                // beneath an already admitted root. Retain every reservation not already covered.
                let consumed = protection.is_some_and(|protection| {
                    if state
                        .protections
                        .iter()
                        .any(|existing| existing.covers(&protection))
                    {
                        false
                    } else {
                        state.protections.push(protection);
                        true
                    }
                });
                return Ok((binding.clone(), consumed));
            }
        }
        let consumed = protection.is_some();
        let binding = SessionMcpLease(Arc::new(Binding {
            pending: Arc::downgrade(&self.pending),
            id: uuid::Uuid::new_v4(),
            session,
            sessions: Arc::downgrade(sessions),
            admission: Arc::downgrade(admission),
            state: Mutex::new(BindingState {
                lease: Some(runtime.acquire(identity)?),
                calls: Vec::new(),
                protections: protection.into_iter().collect(),
                accepted: !delivery,
                pending_deliveries: usize::from(delivery),
            }),
        }));
        bindings.push(binding.clone());
        Ok((binding, consumed))
    }

    pub(crate) fn withdraw_delivery(&self, session: &str, binding: &SessionMcpLease) {
        {
            let mut state = binding.0.state.lock().expect("MCP binding lock poisoned");
            state.pending_deliveries -= 1;
        }
        // Recheck acceptance under the binding fence; another caller can accept between locks.
        self.release_inner(session, Some(binding.id()), true);
    }

    pub(crate) fn release(
        &self,
        session: &str,
        id: Option<uuid::Uuid>,
    ) -> Vec<watch::Receiver<bool>> {
        self.release_inner(session, id, false)
    }

    fn release_inner(
        &self,
        session: &str,
        id: Option<uuid::Uuid>,
        unaccepted_only: bool,
    ) -> Vec<watch::Receiver<bool>> {
        let mut all = self
            .bindings
            .lock()
            .expect("MCP session bindings lock poisoned");
        let mut pending = Vec::new();
        if let Some(bindings) = all.get_mut(session) {
            bindings.retain(|binding| {
                if id.is_some_and(|id| binding.id() != id) {
                    return true;
                }
                let mut state = binding.0.state.lock().expect("MCP binding lock poisoned");
                if unaccepted_only && (state.accepted || state.pending_deliveries != 0) {
                    return true;
                }
                for (cancel, done) in state.calls.drain(..) {
                    cancel.send_replace(true);
                    pending.push(done);
                }
                if !state.protections.is_empty() {
                    let protections = std::mem::take(&mut state.protections);
                    let done = state.lease.as_ref().expect("protected binding owns a lease").resource_settlement();
                    let (released, finished) = watch::channel(false);
                    // Resource release is independent of session close, including explicit unbind.
                    tokio::spawn(async move {
                        settled(vec![done]).await;
                        let mut complete = true;
                        for protection in protections {
                            if let Err(error) = protection.release().await {
                                complete = false;
                                tracing::error!(code = error.code(), "cannot release MCP resource reservation; inspect workspace recovery");
                            }
                        }
                        if complete { released.send_replace(true); }
                    });
                    pending.push(finished.clone());
                    let mut cleanup = self.cleanup.lock().expect("MCP cleanup lock poisoned");
                    cleanup.retain(|done| !*done.borrow());
                    cleanup.push(finished);
                }
                state.lease.take();
                false
            });
            if bindings.is_empty() {
                all.remove(session);
            }
        }
        let mut retired = self
            .retired
            .lock()
            .expect("MCP retired operations lock poisoned");
        let operations = retired.entry(session.to_owned()).or_default();
        operations.retain(|done| !*done.borrow());
        operations.extend(pending.iter().cloned());
        pending
    }

    pub(crate) fn finish(&self, session: &str) -> Vec<watch::Receiver<bool>> {
        let builtin = self
            .operations
            .lock()
            .expect("MCP operations lock poisoned")
            .remove(session)
            .unwrap_or_default();
        builtin.cancel();
        self.configured
            .lock()
            .expect("MCP configured bindings lock poisoned")
            .remove(session);
        self.release(session, None);
        let mut pending = self
            .retired
            .lock()
            .expect("MCP retired operations lock poisoned")
            .remove(session)
            .unwrap_or_default();
        pending.extend(builtin.calls.into_iter().map(|call| call.done));
        pending
    }

    pub(crate) fn cancel(&self, session: &str) {
        if let Some(operations) = self
            .operations
            .lock()
            .expect("MCP operations lock poisoned")
            .get(session)
        {
            operations.cancel();
        }
        let all = self
            .bindings
            .lock()
            .expect("MCP session bindings lock poisoned");
        if let Some(bindings) = all.get(session) {
            for binding in bindings {
                let state = binding.0.state.lock().expect("MCP binding lock poisoned");
                for (cancel, _) in &state.calls {
                    cancel.send_replace(true);
                }
            }
        }
    }

    pub(crate) fn clear(&self) {
        let mut operations = self
            .operations
            .lock()
            .expect("MCP operations lock poisoned");
        let mut cleanup = self.cleanup.lock().expect("MCP cleanup lock poisoned");
        for (_, operations) in operations.drain() {
            operations.cancel();
            for call in operations.calls {
                cleanup.push(call.done);
            }
        }
        drop(cleanup);
        drop(operations);
        self.configured
            .lock()
            .expect("MCP configured bindings lock poisoned")
            .clear();
        let sessions: Vec<_> = self
            .bindings
            .lock()
            .expect("MCP session bindings lock poisoned")
            .keys()
            .cloned()
            .collect();
        for session in sessions {
            self.release(&session, None);
        }
        self.retired
            .lock()
            .expect("MCP retired operations lock poisoned")
            .clear();
    }

    pub(crate) async fn drain_cleanup(&self) {
        let pending = self
            .cleanup
            .lock()
            .expect("MCP cleanup lock poisoned")
            .clone();
        // Shutdown stays bounded on unconfirmed termination/storage failure; persisted holds survive.
        if tokio::time::timeout(std::time::Duration::from_secs(5), settled(pending))
            .await
            .is_err()
        {
            tracing::warn!(
                "MCP resource cleanup remains protected; inspect workspace recovery after restart"
            );
        }
    }
}

pub(crate) async fn settled(pending: Vec<watch::Receiver<bool>>) {
    for mut done in pending {
        while !*done.borrow_and_update() {
            if done.changed().await.is_err() {
                // Lost evidence is not completion, particularly after failed durable release.
                std::future::pending::<()>().await;
            }
        }
    }
}
