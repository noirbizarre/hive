//! File protection is independent of session attachment and survives lost process evidence.

use super::*;
use crate::mcp::{
    AdmissionError, InstanceIdentity, ResolvedTransport, Scope, ScopeContext, SessionMcpLease,
    StdioAdmission, runtime,
};

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Reservation {
    pub(super) session: String,
    pub(super) workspaces: BTreeSet<WorkspaceId>,
}

#[derive(Clone)]
pub(crate) struct Protection {
    service: Service,
    id: uuid::Uuid,
    workspaces: BTreeSet<WorkspaceId>,
}

impl Protection {
    pub(crate) fn covers(&self, other: &Self) -> bool {
        self.workspaces.is_superset(&other.workspaces)
    }
    pub(crate) async fn release(self) -> Result<(), Error> {
        self.service.run(move |rt| rt.release_mcp(self.id)).await
    }
}

impl Service {
    #[cfg(test)]
    pub(crate) async fn tool_capabilities(
        &self,
        session: String,
        context: hive_protocol::workspace::Context,
        cwd: PathBuf,
        authorize: impl Fn() -> Result<(), hive_protocol::workspace_tools::ToolError> + Send + 'static,
    ) -> Result<
        hive_protocol::workspace_tools::Capabilities,
        hive_protocol::workspace_tools::ToolError,
    > {
        let value = self
            .tool_execute(session, context, cwd, None, authorize)
            .await?;
        serde_json::from_value(value)
            .map_err(|_| hive_protocol::workspace_tools::ToolError::Backend)
    }

    pub(crate) async fn tool_execute(
        &self,
        session: String,
        context: hive_protocol::workspace::Context,
        cwd: PathBuf,
        request: Option<hive_core::workspace::backend::vcs::Request>,
        authorize: impl Fn() -> Result<(), hive_protocol::workspace_tools::ToolError> + Send + 'static,
    ) -> Result<serde_json::Value, hive_protocol::workspace_tools::ToolError> {
        use hive_protocol::workspace_tools::{self as tools, ToolError};
        let mutation_admitted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let admitted = mutation_admitted.clone();
        self.run(move |rt| {
            Ok((|| {
                // A queued job must recheck authority after acquiring the resource deletion gate.
                authorize()?;
                // Reject foreign identities before interpreting even a same-spelled local path.
                if context
                    .owner
                    .as_ref()
                    .is_some_and(|owner| owner != &rt.state.node)
                {
                    return Err(ToolError::Owner);
                }
                rt.mcp_scope(&session, Some(&context), &cwd, true)
                    .map_err(|_| ToolError::Scope)?;
                let id = rt.state.uses.get(&session).ok_or(ToolError::Scope)?;
                let entry = rt
                    .state
                    .workspaces
                    .get(id.as_str())
                    .ok_or(ToolError::Scope)?;
                let selection = rt.selection(entry).map_err(|_| ToolError::Scope)?;
                let backend = selection
                    .repository
                    .as_ref()
                    .map(|repository| rt.manager.repository(repository))
                    .transpose()
                    .map_err(|_| ToolError::Backend)?;
                if let Some(request) = request {
                    let repository = selection
                        .repository
                        .as_ref()
                        .ok_or(ToolError::Unsupported)?;
                    let backend = backend.ok_or(ToolError::Unsupported)?;
                    let location = entry.workspace.location().ok_or(ToolError::Scope)?;
                    let fence = || {
                        authorize()
                            .map_err(|_| hive_core::workspace::backend::vcs::Error::Cancelled)?;
                        // A lost blocking-job result after this fence cannot certify no effects.
                        admitted.store(true, std::sync::atomic::Ordering::Release);
                        Ok(())
                    };
                    let result = backend
                        .vcs(
                            hive_core::workspace::backend::vcs::Target {
                                repository,
                                root: &location.root,
                                authorize_mutation: &fence,
                            },
                            &request,
                        )
                        .map_err(ToolError::Vcs)?;
                    let mut value = serde_json::to_value(result).map_err(|_| ToolError::Backend)?;
                    value["schemaVersion"] = serde_json::json!(tools::VERSION);
                    value["operation"] = serde_json::to_value(&request)
                        .map_err(|_| ToolError::Backend)?["operation"]
                        .clone();
                    value["owner"] = serde_json::json!(rt.state.node.to_string());
                    value["project"] =
                        serde_json::json!(entry.workspace.intent().project.to_string());
                    value["workspace"] = serde_json::json!(id.to_string());
                    value["repository"] = serde_json::json!(repository.id.to_string());
                    value["repositoryBackend"] = serde_json::json!(repository.backend.0);
                    return Ok(value);
                }
                serde_json::to_value(tools::Capabilities {
                    schema_version: tools::VERSION,
                    owner: rt.state.node.to_string(),
                    project: entry.workspace.intent().project.to_string(),
                    workspace: id.to_string(),
                    repository_backend: selection
                        .repository
                        .as_ref()
                        .map(|repository| repository.backend.0.clone()),
                    provisioner: selection.provisioner.0.clone(),
                    tools: tools::catalog(backend.is_some_and(|backend| backend.supports_vcs())),
                })
                .map_err(|_| ToolError::Backend)
            })())
        })
        .await
        .map_err(|_| {
            if mutation_admitted.load(std::sync::atomic::Ordering::Acquire) {
                ToolError::Vcs(hive_core::workspace::backend::vcs::Error::Uncertain)
            } else {
                ToolError::Backend
            }
        })?
    }

    /// Trusted recovery boundary: call only after independently verifying termination
    /// of every process admitted under this reservation. Missing sessions are insufficient.
    pub(crate) async fn release_mcp_reservation_after_verified_termination(
        &self,
        id: uuid::Uuid,
    ) -> Result<(), Error> {
        self.run(move |rt| rt.release_mcp(id)).await
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "the final fence needs the pinned session snapshot and runtime"
    )]
    pub(crate) async fn admit_mcp(
        &self,
        broker: crate::Broker,
        runtime: runtime::Runtime,
        session: hive_protocol::v2::SessionId,
        control: Arc<std::sync::Mutex<crate::Control>>,
        context: Option<hive_protocol::workspace::Context>,
        cwd: PathBuf,
        admission: StdioAdmission,
        cancelled: Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<(SessionMcpLease, bool), AdmissionError> {
        let service = self.clone();
        // Keep inspection, reservation publication and final admission under one deletion gate.
        self.run(move |rt| {
            Ok((|| {
                let scoped = admission.definition.scope() != Scope::Machine;
                let (scope, mut root) = rt.mcp_scope(
                    &session.to_string(),
                    scoped.then_some(context.as_ref()).flatten(),
                    &cwd,
                    scoped,
                )?;
                if admission.definition.scope() != Scope::Workspace {
                    root = None;
                }
                let identity = admission.identity(scope, root)?;
                let id = uuid::Uuid::new_v4();
                let resources = rt.mcp_dependencies(&identity)?;
                let protection = if resources.is_empty() {
                    None
                } else {
                    // Persist before acquire: even a crash between save and launch must fail closed.
                    rt.state.mcp_reservations.insert(
                        id.to_string(),
                        Reservation {
                            session: session.to_string(),
                            workspaces: resources.clone(),
                        },
                    );
                    rt.save().map_err(|_| AdmissionError::Storage)?;
                    Some(Protection {
                        service,
                        id,
                        workspaces: resources,
                    })
                };
                let result = broker.finish_scoped_mcp(
                    &session,
                    &runtime,
                    &control,
                    scoped.then_some(context.as_ref()).flatten(),
                    identity,
                    protection,
                    &cancelled,
                );
                if !matches!(&result, Ok((_, true))) {
                    // Only remove a reservation the binding did not consume (e.g. already-covered paths).
                    if rt.release_mcp(id).is_err() {
                        if let Ok((binding, _)) = &result {
                            broker
                                .mcp_sessions
                                .withdraw_delivery(&session.to_string(), binding);
                        }
                        return Err(AdmissionError::Storage);
                    }
                }
                result
            })())
        })
        .await
        .map_err(|_| AdmissionError::Storage)?
    }
}

impl Runtime<'_> {
    pub(super) fn mcp_holds(&self, id: &WorkspaceId) -> bool {
        self.state
            .mcp_reservations
            .values()
            .any(|reservation| reservation.workspaces.contains(id))
    }

    fn release_mcp(&mut self, id: uuid::Uuid) -> Result<(), Error> {
        if let Some(reservation) = self.state.mcp_reservations.remove(&id.to_string()) {
            for workspace in reservation.workspaces {
                if !self.has_use(&workspace) {
                    self.touch(&workspace);
                }
            }
            self.save()?;
        }
        Ok(())
    }

    pub(super) fn mcp_scope(
        &self,
        session: &str,
        context: Option<&hive_protocol::workspace::Context>,
        cwd: &Path,
        scoped: bool,
    ) -> Result<(ScopeContext, Option<NodePath>), AdmissionError> {
        let mut scope = ScopeContext {
            node: self.state.node.clone(),
            project: None,
            workspace: None,
        };
        if !scoped {
            return Ok((scope, None));
        }
        let context = context.ok_or(AdmissionError::Scope)?;
        if context
            .owner
            .as_ref()
            .is_some_and(|owner| owner != &self.state.node)
        {
            return Err(AdmissionError::Scope);
        }
        let id = self.state.uses.get(session).ok_or(AdmissionError::Scope)?;
        let entry = self
            .state
            .workspaces
            .get(id.as_str())
            .ok_or(AdmissionError::Scope)?;
        if id.as_str() != context.workspace
            || entry.workspace.intent().project.as_str() != context.project
            || entry
                .workspace
                .intent()
                .check_node(&self.state.node)
                .is_err()
            || entry.workspace.state() != WorkspaceState::Ready
            || !matches!(self.inspect(id), Ok(Inspection::Present { .. }))
        {
            return Err(AdmissionError::Scope);
        }
        let location = entry.workspace.location().ok_or(AdmissionError::Scope)?;
        let root = Path::new(location.root.as_str())
            .canonicalize()
            .map_err(|_| AdmissionError::Scope)?;
        let cwd = cwd.canonicalize().map_err(|_| AdmissionError::Scope)?;
        if !cwd.starts_with(&root) {
            return Err(AdmissionError::Scope);
        }
        // Duplicate concrete registrations cannot be silently collapsed into one WorkspaceId.
        if self
            .state
            .workspaces
            .values()
            .filter(|other| other.workspace.state() != WorkspaceState::Removed)
            .filter_map(|other| other.workspace.location())
            .filter(|location| {
                Path::new(location.root.as_str())
                    .canonicalize()
                    .ok()
                    .as_ref()
                    == Some(&root)
            })
            .count()
            != 1
        {
            return Err(AdmissionError::Scope);
        }
        let selection = self.selection(entry).map_err(|_| AdmissionError::Scope)?;
        let native_jj = selection
            .repository
            .as_ref()
            .is_some_and(|repository| repository.backend.0 == "jj");
        for directory in cwd
            .ancestors()
            .take_while(|directory| directory.starts_with(&root))
        {
            if directory.join(".jj").exists() && (!native_jj || directory != root) {
                return Err(AdmissionError::Scope);
            }
            if directory != root && directory.join(".git").exists() {
                return Err(AdmissionError::Scope);
            }
        }
        if !native_jj {
            // Discovery is evidence only; a non-Git directory is valid unless an SCM boundary exists.
            if let Ok(observed) = activity::short_git(&cwd, &["rev-parse", "--show-toplevel"]) {
                if Path::new(observed.trim())
                    .canonicalize()
                    .map_err(|_| AdmissionError::Scope)?
                    != root
                {
                    return Err(AdmissionError::Scope);
                }
            } else if root
                .ancestors()
                .any(|path| path.join(".git").exists() || path.join(".jj").exists())
            {
                return Err(AdmissionError::Scope);
            }
        }
        scope.project = Some(entry.workspace.intent().project.clone());
        scope.workspace = Some(id.clone());
        Ok((
            scope,
            Some(self.path(&root).map_err(|_| AdmissionError::Path)?),
        ))
    }

    pub(super) fn mcp_dependencies(
        &self,
        identity: &InstanceIdentity,
    ) -> Result<BTreeSet<WorkspaceId>, AdmissionError> {
        let ResolvedTransport::Stdio {
            executable, cwd, ..
        } = &identity.transport
        else {
            return Err(runtime::RuntimeError::Inputs.into());
        };
        let mut resources = BTreeSet::new();
        for entry in self
            .state
            .workspaces
            .values()
            .filter(|entry| entry.workspace.state() != WorkspaceState::Removed)
        {
            let root = entry
                .workspace
                .location()
                .map(|location| PathBuf::from(location.root.as_str()))
                .or_else(|| {
                    self.state
                        .operations
                        .values()
                        .find(|operation| {
                            operation.workspace == *entry.workspace.id() && operation.is_create()
                        })
                        .and_then(|operation| operation.path.clone())
                });
            let Some(root) = root else {
                continue;
            };
            let root = root.canonicalize().unwrap_or(root);
            let exposed = std::iter::once(cwd)
                .chain(identity.initialization.roots.iter())
                .any(|path| {
                    let path = Path::new(path.as_str());
                    path.starts_with(&root) || root.starts_with(path)
                })
                || Path::new(executable.as_str()).starts_with(&root);
            let concrete = matches!(&identity.scope, crate::mcp::ScopeIdentity::Workspace(_, _, id) if id == entry.workspace.id());
            if concrete
                || (exposed && entry.workspace.intent().ownership() == Ownership::HiveManaged)
            {
                if entry.workspace.state() != WorkspaceState::Ready
                    || !matches!(
                        self.inspect(entry.workspace.id()),
                        Ok(Inspection::Present { .. })
                    )
                {
                    return Err(AdmissionError::Scope);
                }
                resources.insert(entry.workspace.id().clone());
            }
        }
        Ok(resources)
    }
}
