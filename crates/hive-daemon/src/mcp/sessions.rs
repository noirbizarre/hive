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
pub(crate) struct Sessions(
    Mutex<HashMap<String, Vec<SessionMcpLease>>>,
    Mutex<HashMap<String, Vec<watch::Receiver<bool>>>>,
);

struct Binding {
    id: uuid::Uuid,
    session: String,
    sessions: Weak<Mutex<HashMap<String, crate::Entry>>>,
    admission: Weak<Mutex<crate::reload::Registry>>,
    state: Mutex<BindingState>,
}

struct BindingState {
    lease: Option<Lease>,
    calls: Vec<(watch::Sender<bool>, watch::Receiver<bool>)>,
}

/// Revocable access; the Broker retains ownership even when every caller drops its handle.
#[derive(Clone)]
pub struct SessionMcpLease(Arc<Binding>);

impl SessionMcpLease {
    pub fn id(&self) -> uuid::Uuid {
        self.0.id
    }

    pub fn request(&self, request: McpRequest) -> Result<McpCall, RuntimeError> {
        let admission = self.0.admission.upgrade().ok_or(RuntimeError::Binding)?;
        // Match Broker admission lock order so shutdown fences bindings and calls together.
        let admission = admission.lock().map_err(|_| RuntimeError::Binding)?;
        if admission.stopping {
            return Err(RuntimeError::Stopping);
        }
        let sessions = self.0.sessions.upgrade().ok_or(RuntimeError::Binding)?;
        let sessions = sessions.lock().map_err(|_| RuntimeError::Binding)?;
        let entry = sessions.get(&self.0.session).ok_or(RuntimeError::Binding)?;
        let control = entry.control.lock().map_err(|_| RuntimeError::Binding)?;
        if control.closing || control.activity.session == "ended" {
            return Err(RuntimeError::Binding);
        }
        let mut state = self.0.state.lock().map_err(|_| RuntimeError::Binding)?;
        let lease = state.lease.as_ref().ok_or(RuntimeError::Binding)?;
        // Register cancellation under the close fence before the spawned request can publish.
        let call = lease.request(request);
        state.calls.retain(|(_, done)| !*done.borrow());
        state.calls.push((call.cancel.clone(), call.done.clone()));
        Ok(call)
    }
}

impl Sessions {
    pub(crate) fn bind(
        &self,
        session: String,
        sessions: &Arc<Mutex<HashMap<String, crate::Entry>>>,
        admission: &Arc<Mutex<crate::reload::Registry>>,
        runtime: &Runtime,
        identity: InstanceIdentity,
    ) -> Result<SessionMcpLease, RuntimeError> {
        let mut all = self.0.lock().map_err(|_| RuntimeError::Binding)?;
        let bindings = all.entry(session.clone()).or_default();
        for binding in bindings.iter() {
            let state = binding.0.state.lock().map_err(|_| RuntimeError::Binding)?;
            if state
                .lease
                .as_ref()
                .is_some_and(|lease| lease.identity() == &identity)
            {
                return Ok(binding.clone());
            }
        }
        let binding = SessionMcpLease(Arc::new(Binding {
            id: uuid::Uuid::new_v4(),
            session,
            sessions: Arc::downgrade(sessions),
            admission: Arc::downgrade(admission),
            state: Mutex::new(BindingState {
                lease: Some(runtime.acquire(identity)?),
                calls: Vec::new(),
            }),
        }));
        bindings.push(binding.clone());
        Ok(binding)
    }

    pub(crate) fn release(
        &self,
        session: &str,
        id: Option<uuid::Uuid>,
    ) -> Vec<watch::Receiver<bool>> {
        let mut all = self.0.lock().expect("MCP session bindings lock poisoned");
        let mut pending = Vec::new();
        if let Some(bindings) = all.get_mut(session) {
            bindings.retain(|binding| {
                if id.is_some_and(|id| binding.id() != id) {
                    return true;
                }
                let mut state = binding.0.state.lock().expect("MCP binding lock poisoned");
                for (cancel, done) in state.calls.drain(..) {
                    cancel.send_replace(true);
                    pending.push(done);
                }
                state.lease.take();
                false
            });
            if bindings.is_empty() {
                all.remove(session);
            }
        }
        let mut retired = self.1.lock().expect("MCP retired operations lock poisoned");
        let operations = retired.entry(session.to_owned()).or_default();
        operations.retain(|done| !*done.borrow());
        operations.extend(pending.iter().cloned());
        pending
    }

    pub(crate) fn finish(&self, session: &str) -> Vec<watch::Receiver<bool>> {
        self.release(session, None);
        self.1
            .lock()
            .expect("MCP retired operations lock poisoned")
            .remove(session)
            .unwrap_or_default()
    }

    pub(crate) fn cancel(&self, session: &str) {
        let all = self.0.lock().expect("MCP session bindings lock poisoned");
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
        let sessions: Vec<_> = self
            .0
            .lock()
            .expect("MCP session bindings lock poisoned")
            .keys()
            .cloned()
            .collect();
        for session in sessions {
            self.release(&session, None);
        }
        self.1
            .lock()
            .expect("MCP retired operations lock poisoned")
            .clear();
    }
}

pub(crate) async fn settled(pending: Vec<watch::Receiver<bool>>) {
    for mut done in pending {
        while !*done.borrow_and_update() {
            if done.changed().await.is_err() {
                break;
            }
        }
    }
}
