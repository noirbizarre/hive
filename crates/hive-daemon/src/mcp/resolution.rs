//! Admission observes local resources once, then launches exactly that immutable snapshot.

use std::{collections::BTreeMap, ffi::OsString, path::Path};

use hive_core::{
    identity::NodeId,
    workspace::{NodePath, PathSyntax},
};
use hive_protocol::v2;
use miette::Diagnostic;

use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

struct CancelAdmission(Arc<AtomicBool>);
impl Drop for CancelAdmission {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Sharing {
    Shared,
    Private,
}

#[derive(Clone)]
pub struct StdioLaunch {
    pub executable: NodePath,
    pub env: BTreeMap<OsString, OsString>,
    /// None selects the verified concrete root only for workspace scope.
    pub cwd: Option<NodePath>,
}

#[derive(Clone)]
pub struct StdioAdmission {
    pub definition_id: String,
    pub definition: ServerDefinition,
    pub launch: StdioLaunch,
    pub initialization: Initialization,
    pub security: SecurityContext,
    pub sharing: Sharing,
}

impl fmt::Debug for StdioLaunch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("StdioLaunch([redacted])")
    }
}
impl fmt::Debug for StdioAdmission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("StdioAdmission([redacted])")
    }
}

#[derive(Debug, thiserror::Error, Diagnostic)]
pub enum AdmissionError {
    #[error(
        "MCP scope cannot be verified; attach the session to an unambiguous registered concrete workspace on this node and ensure its project still exists"
    )]
    #[diagnostic(code(hive::mcp::admission_scope))]
    Scope,
    #[error(
        "MCP launch path cannot be observed on this node; supply existing node-local executable/directories and an explicit cwd for machine/project scope"
    )]
    #[diagnostic(code(hive::mcp::admission_path))]
    Path,
    #[error(
        "MCP resource protection could not be persisted; inspect workspace recovery reservations and check state storage before retrying"
    )]
    #[diagnostic(code(hive::mcp::admission_storage))]
    Storage,
    #[error(transparent)]
    #[diagnostic(transparent)]
    Contract(#[from] McpError),
    #[error(transparent)]
    #[diagnostic(transparent)]
    Runtime(#[from] runtime::RuntimeError),
}

pub(crate) fn canonical(
    node: &NodeId,
    path: &NodePath,
    directory: bool,
) -> Result<NodePath, AdmissionError> {
    let syntax = if cfg!(windows) {
        PathSyntax::Windows
    } else {
        PathSyntax::Posix
    };
    if path.node() != node || path.syntax() != syntax {
        return Err(AdmissionError::Path);
    }
    let path = Path::new(path.as_str())
        .canonicalize()
        .map_err(|_| AdmissionError::Path)?;
    if if directory {
        !path.is_dir()
    } else {
        !path.is_file()
    } {
        return Err(AdmissionError::Path);
    }
    NodePath::new(
        node.clone(),
        syntax,
        path.to_str().ok_or(AdmissionError::Path)?.into(),
    )
    .map_err(|_| AdmissionError::Path)
}

impl StdioAdmission {
    pub(crate) fn identity(
        mut self,
        context: ScopeContext,
        root: Option<NodePath>,
    ) -> Result<InstanceIdentity, AdmissionError> {
        let Transport::Stdio { args, .. } = self.definition.transport() else {
            return Err(runtime::RuntimeError::Inputs.into());
        };
        if self.definition.ownership() != Ownership::Managed {
            return Err(runtime::RuntimeError::Inputs.into());
        }
        let cwd = self.launch.cwd.or(root).ok_or(AdmissionError::Path)?;
        let transport = ResolvedTransport::Stdio {
            executable: canonical(&context.node, &self.launch.executable, false)?,
            args: args.clone(),
            env: self.launch.env,
            cwd: canonical(&context.node, &cwd, true)?,
        };
        for root in &mut self.initialization.roots {
            *root = canonical(&context.node, root, true)?;
        }
        let build = match self.sharing {
            Sharing::Shared => InstanceIdentity::shared,
            Sharing::Private => InstanceIdentity::private,
        };
        Ok(build(
            self.definition_id,
            self.definition,
            context,
            transport,
            self.initialization,
            self.security,
        )?)
    }
}

struct Delivery {
    broker: crate::Broker,
    session: v2::SessionId,
    lease: SessionMcpLease,
    pending: bool,
}

impl Delivery {
    fn accept(mut self) -> SessionMcpLease {
        self.lease.accept_delivery();
        self.pending = false;
        self.lease.clone()
    }
}

impl Drop for Delivery {
    fn drop(&mut self) {
        // Also covers cancellation after send succeeded but before the receiver consumed it.
        if self.pending {
            self.broker
                .mcp_sessions
                .withdraw_delivery(&self.session.to_string(), &self.lease);
        }
    }
}

impl crate::Broker {
    /// Reconcile a previous daemon's reservation after independently verifying all
    /// admitted processes terminated. Call on a fresh Broker before initializing MCP.
    pub async fn release_mcp_reservation_after_verified_termination(
        &self,
        id: uuid::Uuid,
    ) -> Result<(), AdmissionError> {
        let _admission = self.mcp_sessions.admission.lock().await;
        // A live/restart-capable local binding must never lose its resource protection.
        if self.mcp_runtime.get().is_some() {
            return Err(runtime::RuntimeError::Binding.into());
        }
        self.workspace_service()
            .map_err(|_| AdmissionError::Storage)?
            .release_mcp_reservation_after_verified_termination(id)
            .await
            .map_err(|_| AdmissionError::Storage)
    }

    /// Resolve scope from registered session resources; executable/env/security remain trusted inputs.
    pub async fn bind_scoped_mcp(
        &self,
        session: &v2::SessionId,
        admission: StdioAdmission,
    ) -> Result<SessionMcpLease, AdmissionError> {
        let broker = self.clone();
        let session = session.clone();
        let cancelled = Arc::new(AtomicBool::new(false));
        let _cancel_on_drop = CancelAdmission(cancelled.clone());
        let (reply, received) = tokio::sync::oneshot::channel();
        // The coordinator outlives a cancelled caller, including a resource job already in flight.
        tokio::spawn(async move {
            let _admission = broker.mcp_sessions.admission.lock().await;
            if reply.is_closed() {
                return;
            }
            let result = broker
                .admit_scoped_mcp(&session, admission, cancelled)
                .await
                .map(|(lease, _)| Delivery {
                    broker: broker.clone(),
                    session,
                    lease,
                    pending: true,
                });
            let _ = reply.send(result);
        });
        received
            .await
            .map_err(|_| AdmissionError::Storage)?
            .map(Delivery::accept)
    }

    async fn admit_scoped_mcp(
        &self,
        session: &v2::SessionId,
        admission: StdioAdmission,
        cancelled: Arc<AtomicBool>,
    ) -> Result<(SessionMcpLease, bool), AdmissionError> {
        // Queued coordinators must not initialize a runtime or reserve files after shutdown.
        if self
            .registry
            .lock()
            .map_err(|_| runtime::RuntimeError::Binding)?
            .stopping
        {
            return Err(runtime::RuntimeError::Stopping.into());
        }
        let runtime = self.managed_mcp().await?.clone();
        let (control, context, cwd) = {
            let sessions = self
                .sessions
                .lock()
                .map_err(|_| runtime::RuntimeError::Binding)?;
            let entry = sessions
                .get(&session.to_string())
                .ok_or(runtime::RuntimeError::Binding)?;
            let state = entry
                .control
                .lock()
                .map_err(|_| runtime::RuntimeError::Binding)?;
            if state.closing || state.activity.session == "ended" {
                return Err(runtime::RuntimeError::Binding.into());
            }
            // A same-spelled remote cwd is not evidence about this node's filesystem.
            if admission.definition.scope() != Scope::Machine && !state.worktrunk_local {
                return Err(AdmissionError::Scope);
            }
            (
                entry.control.clone(),
                state.workspace_context.clone(),
                entry.cwd.clone(),
            )
        };
        if let Some(store) = &self.store {
            crate::workspaces::Service::new(store.clone())
                .admit_mcp(
                    self.clone(),
                    runtime,
                    session.clone(),
                    control,
                    context,
                    cwd.0,
                    admission,
                    cancelled,
                )
                .await
        } else {
            if admission.definition.scope() != Scope::Machine {
                return Err(AdmissionError::Scope);
            }
            let node = runtime.node().clone();
            let identity = tokio::task::spawn_blocking(move || {
                admission.identity(
                    ScopeContext {
                        node,
                        project: None,
                        workspace: None,
                    },
                    None,
                )
            })
            .await
            .map_err(|_| AdmissionError::Storage)??;
            self.finish_scoped_mcp(
                session, &runtime, &control, None, identity, None, &cancelled,
            )
        }
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "final admission compares the pinned session and transfers resource ownership"
    )]
    pub(crate) fn finish_scoped_mcp(
        &self,
        session: &v2::SessionId,
        runtime: &runtime::Runtime,
        expected: &std::sync::Arc<std::sync::Mutex<crate::Control>>,
        context: Option<&hive_protocol::workspace::Context>,
        identity: InstanceIdentity,
        reservation: Option<crate::workspaces::mcp::Protection>,
        cancelled: &AtomicBool,
    ) -> Result<(SessionMcpLease, bool), AdmissionError> {
        let registry = self
            .registry
            .lock()
            .map_err(|_| runtime::RuntimeError::Binding)?;
        if registry.stopping {
            return Err(runtime::RuntimeError::Stopping.into());
        }
        let sessions = self
            .sessions
            .lock()
            .map_err(|_| runtime::RuntimeError::Binding)?;
        let entry = sessions
            .get(&session.to_string())
            .ok_or(runtime::RuntimeError::Binding)?;
        if !std::sync::Arc::ptr_eq(&entry.control, expected) {
            return Err(runtime::RuntimeError::Binding.into());
        }
        let control = entry
            .control
            .lock()
            .map_err(|_| runtime::RuntimeError::Binding)?;
        if control.closing || control.activity.session == "ended" {
            return Err(runtime::RuntimeError::Binding.into());
        }
        if let Some(context) = context
            && (control.workspace_context.as_ref().is_none_or(|current| {
                current.project != context.project || current.workspace != context.workspace
            }) || !registry
                .current
                .projects
                .keys()
                .chain(registry.managed_projects.keys())
                .any(|id| id.as_str() == context.project))
        {
            return Err(AdmissionError::Scope);
        }
        if cancelled.load(Ordering::Acquire) {
            return Err(runtime::RuntimeError::Cancelled.into());
        }
        Ok(self.mcp_sessions.bind_protected(
            session.to_string(),
            &self.sessions,
            &self.registry,
            runtime,
            identity,
            reservation,
        )?)
    }
}
