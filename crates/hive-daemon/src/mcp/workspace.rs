//! In-process tools inherit authority from the owner-side ACP session attachment.

use std::sync::{Arc, Mutex};

use agent_client_protocol::mcp_server::McpOperationCancellation;
use hive_protocol::{
    v2,
    workspace::Context,
    workspace_tools::{Capabilities, ToolError},
};

use super::runtime::RuntimeError;

struct CancelOnDrop(Arc<std::sync::atomic::AtomicBool>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Release);
    }
}

#[derive(Clone)]
pub(crate) struct Provider {
    broker: crate::Broker,
    session: v2::SessionId,
    control: Arc<Mutex<crate::Control>>,
    context: Context,
    cwd: v2::AbsolutePath,
    active: Arc<std::sync::atomic::AtomicBool>,
    #[cfg(test)]
    pub(crate) before_inspect: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl Provider {
    pub(crate) fn prepare(
        broker: &crate::Broker,
        session: &v2::SessionId,
    ) -> Result<Option<Self>, RuntimeError> {
        let sessions = broker.sessions.lock().map_err(|_| RuntimeError::Binding)?;
        let pending = broker
            .mcp_sessions
            .pending
            .lock()
            .map_err(|_| RuntimeError::Binding)?;
        let (control, cwd) = sessions
            .get(&session.to_string())
            .map(|e| (&e.control, &e.cwd))
            .or_else(|| {
                pending
                    .get(&session.to_string())
                    .map(|e| (&e.control, &e.cwd))
            })
            .ok_or(RuntimeError::Binding)?;
        let state = control.lock().map_err(|_| RuntimeError::Binding)?;
        let Some(context) = state
            .workspace_context
            .clone()
            .filter(|_| state.worktrunk_local)
        else {
            return Ok(None);
        };
        Ok(Some(Self {
            broker: broker.clone(),
            session: session.clone(),
            control: control.clone(),
            context,
            cwd: cwd.clone(),
            active: broker.mcp_sessions.bind_provider(&session.to_string())?,
            #[cfg(test)]
            before_inspect: None,
        }))
    }

    fn with_authority<T>(
        &self,
        work: impl FnOnce() -> Result<T, RuntimeError>,
    ) -> Result<T, RuntimeError> {
        super::sessions::with_authority(
            &self.broker.registry,
            &self.broker.sessions,
            &self.broker.mcp_sessions.pending,
            &self.session.to_string(),
            Some(&self.control),
            |state, registry| {
                if !self.active.load(std::sync::atomic::Ordering::Acquire) {
                    return Err(ToolError::Session.into());
                }
                if !state.worktrunk_local
                    || state.workspace_context.as_ref() != Some(&self.context)
                    || !registry
                        .current
                        .projects
                        .keys()
                        .chain(registry.managed_projects.keys())
                        .any(|id| id.as_str() == self.context.project)
                {
                    return Err(ToolError::Scope.into());
                }
                work()
            },
        )
        .map_err(|error| match error {
            RuntimeError::Binding | RuntimeError::Stopping => ToolError::Session.into(),
            error => error,
        })
    }

    pub(crate) async fn capabilities(
        &self,
        cancellation: Option<&McpOperationCancellation>,
    ) -> Result<Capabilities, RuntimeError> {
        if cancellation.is_some_and(McpOperationCancellation::is_cancelled) {
            return Err(ToolError::Cancelled.into());
        }
        let (mut cancelled, done) = self.with_authority(|| {
            self.broker
                .mcp_sessions
                .track_operation(&self.session.to_string(), &self.active)
        })?;
        let this = self.clone();
        let (reply, response) = tokio::sync::oneshot::channel();
        let admission_cancelled = cancelled.clone();
        let caller_cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cancel_on_drop = CancelOnDrop(caller_cancelled.clone());
        // This task owns settlement even when the SDK drops the awaiting request future.
        tokio::spawn(async move {
            let result = if *admission_cancelled.borrow() {
                Err(ToolError::Cancelled)
            } else {
                match this.broker.workspace_service() {
                    Ok(service) => {
                        let authority = this.clone();
                        service
                            .tool_capabilities(
                                this.session.to_string(),
                                this.context,
                                this.cwd.0,
                                move || {
                                    if *admission_cancelled.borrow()
                                        || caller_cancelled
                                            .load(std::sync::atomic::Ordering::Acquire)
                                    {
                                        return Err(ToolError::Cancelled);
                                    }
                                    authority.with_authority(|| Ok(())).map_err(
                                        |error| match error {
                                            RuntimeError::WorkspaceTool(error) => error,
                                            _ => ToolError::Session,
                                        },
                                    )?;
                                    #[cfg(test)]
                                    if let Some(hook) = &authority.before_inspect {
                                        hook();
                                    }
                                    Ok(())
                                },
                            )
                            .await
                    }
                    Err(_) => Err(ToolError::Backend),
                }
            };
            done.send_replace(true);
            let _ = reply.send(result);
        });
        tokio::pin!(response);
        // Cancellation never drops the resource job: wait for the coordinator's actual settlement.
        let result = tokio::select! {
            result = &mut response => Some(result),
            _ = async { match cancellation { Some(cancel) => cancel.cancelled().await, None => std::future::pending().await } } => None,
            _ = async { let _ = cancelled.wait_for(|cancelled| *cancelled).await; } => None,
        };
        match result {
            Some(result) => result.map_err(|_| ToolError::Backend)?.map_err(Into::into),
            None => {
                drop(cancel_on_drop);
                let _ = response.await;
                Err(ToolError::Cancelled.into())
            }
        }
    }
}
