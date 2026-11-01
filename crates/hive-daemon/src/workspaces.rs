//! Node-local Git resources outlive sessions; all mutations retain durable intent.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::OpenOptions,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
    time::Duration,
};

use crate::{config::WorkspaceTemplate, persistence::Store};
use hive_core::{
    identity::{NodeId, OperationId, ProjectId, RepositoryId, WorkspaceId},
    project::{NodeRepository, ProjectDefinition, RepositoryRole},
    workspace::{
        BackendId, GitIntent, GitRef, ManagedBacking, NodePath, Ownership, PathSyntax, Retention,
        RetryDisposition, Workspace, WorkspaceEvent, WorkspaceIntent, WorkspaceLocation,
        WorkspacePolicy, WorkspaceState,
        backend::{
            Cleanliness, CleanupMode, CleanupRequest, Inspection, ResourceTarget, ResourceUse,
            SafetyEvidence,
        },
        integration::{IntegrationSelection, WorktrunkAvailability, WorktrunkConfig},
    },
};
use serde::{Deserialize, Serialize};
mod routes;
pub(crate) use routes::router;

impl crate::Broker {
    pub(crate) async fn remove_workspace(
        &self,
        id: WorkspaceId,
        operation: OperationId,
        discard: bool,
    ) -> Result<Entry, Error> {
        let broker = self.clone();
        self.workspace_service()?
            .run(move |rt| {
                rt.reclaim_unstarted(&broker)?;
                rt.remove(id, operation, discard, None)
            })
            .await
    }

    pub(crate) async fn reconcile_workspace(
        &self,
        id: WorkspaceId,
        directory: Option<PathBuf>,
        hooks_reconciled: bool,
    ) -> Result<Entry, Error> {
        let broker = self.clone();
        self.workspace_service()?
            .run(move |rt| {
                rt.reclaim_unstarted(&broker)?;
                rt.reconcile(id, directory, hooks_reconciled)
            })
            .await
    }
    pub(crate) fn workspace_service(&self) -> Result<Service, Error> {
        Ok(Service::new(self.store.clone().ok_or(Error::Storage)?))
    }

    pub(crate) async fn launch_workspace(
        &self,
        request: &mut hive_protocol::v2::NewSessionRequest,
        launch: Option<hive_protocol::workspace::Launch>,
        profile: &crate::config::Profile,
        config: Arc<crate::config::Config>,
        managed: BTreeMap<ProjectId, ProjectDefinition>,
        session: String,
    ) -> Result<Option<hive_protocol::workspace::Context>, Error> {
        use hive_protocol::workspace::Launch;
        let launch = launch.or_else(|| {
            profile.workspace.as_ref().map(|_| Launch::New {
                project: None,
                policy: None,
                base_ref: None,
                branch: None,
            })
        });
        let Some(launch) = launch else {
            if let Some(store) = &self.store {
                return Service::new(store.clone())
                    .track_directory(request.cwd.0.clone(), session)
                    .await;
            }
            return Ok(None);
        };
        if let Launch::Existing { directory } = launch {
            let path = PathBuf::from(directory);
            if !path.is_absolute() || !path.is_dir() {
                return Err(Error::Request);
            }
            request.cwd = hive_protocol::v2::AbsolutePath::new(path.clone());
            if let Some(store) = &self.store {
                return Service::new(store.clone())
                    .track_directory(path, session)
                    .await;
            }
            return Ok(None);
        }
        let service = self.workspace_service()?;
        let entry = match launch {
            Launch::Attach { id } => {
                service
                    .attach(
                        WorkspaceId::try_from(id).map_err(|_| Error::Request)?,
                        session,
                    )
                    .await?
            }
            Launch::New {
                project,
                policy,
                base_ref,
                branch,
            } => {
                let project = project
                    .map(ProjectId::try_from)
                    .transpose()
                    .map_err(|_| Error::Request)?
                    .or_else(|| profile.project.clone())
                    .ok_or(Error::Request)?;
                let policy = policy
                    .or_else(|| profile.workspace.clone())
                    .ok_or(Error::Request)?;
                let policy = config
                    .workspaces
                    .get(&policy)
                    .cloned()
                    .ok_or(Error::Request)?;
                let definition = config
                    .projects
                    .get(&project)
                    .or_else(|| managed.get(&project))
                    .cloned()
                    .ok_or(Error::Unknown)?;
                let create = CreateRequest {
                    operation: OperationId::new(),
                    project,
                    policy,
                    base_ref: base_ref.map(GitRef::try_from).transpose()?,
                    branch: branch.map(GitRef::try_from).transpose()?,
                };
                service
                    .create(
                        create,
                        definition,
                        config.worktrunk.unwrap_or_default(),
                        Some(session),
                    )
                    .await?
            }
            Launch::Existing { .. } => unreachable!(),
        };
        let cwd = entry
            .workspace
            .location()
            .ok_or(Error::Conflict)?
            .working_directory
            .as_str()
            .to_owned();
        request.cwd = hive_protocol::v2::AbsolutePath::new(PathBuf::from(&cwd));
        Ok(Some(hive_protocol::workspace::Context {
            project: entry.workspace.intent().project.to_string(),
            workspace: entry.workspace.id().to_string(),
            effective_cwd: cwd,
        }))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(
        "workspace registry unavailable; use a persistent daemon and check state permissions/disk space"
    )]
    Storage,
    #[error(
        "resource is not registered on this node; list repositories/workspaces and check the ID"
    )]
    Unknown,
    #[error(
        "Git {0} failed; check the remote, node-local credentials and repository permissions, then inspect and retry"
    )]
    Git(&'static str),
    #[error(
        "remote authentication failed; configure the execution node's Git credential helper or SSH credentials and retry"
    )]
    Authentication,
    #[error(
        "command interrupted or timed out; inspect the operation before retrying, as filesystem or hook effects may exist"
    )]
    Uncertain,
    #[error(
        "resource identity conflicts with the recorded repository, path or branch; inspect the resource before reconciling"
    )]
    Conflict,
    #[error(
        "operation key has different durable intent; replay the original request or use a new operation key"
    )]
    RetryConflict,
    #[error(
        "invalid workspace policy, path or ref; check the configured project, policy and requested branch/base"
    )]
    Request,
    #[error(
        "WorkTrunk command failed; check wt compatibility, hook approvals and project hooks, then inspect before retrying"
    )]
    Worktrunk,
    #[error(transparent)]
    Domain(#[from] hive_core::workspace::WorkspaceError),
}

impl Error {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Storage => "hive::workspace::storage",
            Self::Unknown => "hive::workspace::unknown",
            Self::Git("fetch") => "hive::repository::fetch",
            Self::Git("clone") => "hive::repository::clone",
            Self::Git(_) => "hive::workspace::git",
            Self::Authentication => "hive::repository::authentication",
            Self::Uncertain => "hive::workspace::uncertain_operation",
            Self::Conflict => "hive::workspace::identity_conflict",
            Self::RetryConflict => "hive::workspace::retry_conflict",
            Self::Request => "hive::workspace::request",
            Self::Worktrunk => "hive::workspace::worktrunk",
            Self::Domain(error) => error.code(),
        }
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::json!({"code":self.code(),"message":self.to_string(),"help":"inspect the resource and original operation before retrying; configure credentials and hook approvals on the execution node"})
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Repository {
    #[serde(flatten)]
    pub repository: NodeRepository,
    pub source: String,
    pub status: String,
    pub failure: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Entry {
    pub workspace: Workspace,
    pub repository: Option<RepositoryId>,
    pub failure: Option<String>,
    #[serde(default)]
    pub archive: Option<NodePath>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateRequest {
    pub operation: OperationId,
    pub project: ProjectId,
    pub policy: WorkspaceTemplate,
    pub base_ref: Option<GitRef>,
    pub branch: Option<GitRef>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Operation {
    request: serde_json::Value,
    workspace: WorkspaceId,
    // Pin resolution before creating branches, so a retry cannot silently change its base.
    commit: Option<String>,
    path: Option<PathBuf>,
    effects_started: bool,
}

#[derive(Clone, Serialize, Deserialize)]
struct ArchivePlan {
    destination: PathBuf,
    staging: PathBuf,
    head: String,
    refs: String,
}

#[derive(Serialize, Deserialize)]
struct State {
    node: NodeId,
    repositories: BTreeMap<String, Repository>,
    workspaces: BTreeMap<String, Entry>,
    operations: BTreeMap<String, Operation>,
    // Use survives disconnected clients and daemon restarts; only confirmed close releases it.
    uses: BTreeMap<String, WorkspaceId>,
    // A session may enter a managed checkout through an existing-directory alias.
    #[serde(default)]
    protected: BTreeMap<String, BTreeSet<WorkspaceId>>,
    #[serde(default)]
    archives: BTreeMap<String, ArchivePlan>,
    // A missing catalog row is not proof of idleness once downstream dispatch was possible.
    #[serde(default)]
    dispatched: BTreeSet<String>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            node: NodeId::new(),
            repositories: BTreeMap::new(),
            workspaces: BTreeMap::new(),
            operations: BTreeMap::new(),
            uses: BTreeMap::new(),
            protected: BTreeMap::new(),
            archives: BTreeMap::new(),
            dispatched: BTreeSet::new(),
        }
    }
}

/// The blocking job owns the filesystem guard even if the HTTP/ACP caller disconnects.
#[derive(Clone)]
pub struct Service {
    store: Arc<Store>,
    #[cfg(test)]
    git_config: Option<PathBuf>,
}

#[cfg(test)]
thread_local! { static TEST_GIT_CONFIG: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) }; }

impl Service {
    pub fn new(store: Arc<Store>) -> Self {
        #[cfg(test)]
        let git_config = store.resource("test_git_config").unwrap_or(None);
        Self {
            store,
            #[cfg(test)]
            git_config,
        }
    }

    async fn run<T: Send + 'static>(
        &self,
        work: impl FnOnce(&mut Runtime<'_>) -> Result<T, Error> + Send + 'static,
    ) -> Result<T, Error> {
        let store = self.store.clone();
        #[cfg(test)]
        let config = self.git_config.clone();
        tokio::task::spawn_blocking(move || {
            #[cfg(test)]
            TEST_GIT_CONFIG.with(|current| *current.borrow_mut() = config);
            let lock = OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(store.state_directory().join("resources.lock"))
                .map_err(|_| Error::Storage)?;
            // One cross-process admission gate also excludes cleanup/attachment races. No SQLite lock is held over subprocesses.
            fs2::FileExt::lock_exclusive(&lock).map_err(|_| Error::Storage)?;
            let stored = store.resource::<State>("node")?;
            let initialize = stored.is_none();
            let state = stored.unwrap_or_default();
            let mut runtime = Runtime {
                store: &store,
                state,
            };
            if initialize {
                runtime.save()?;
            }
            work(&mut runtime)
        })
        .await
        .map_err(|_| Error::Uncertain)?
    }

    pub async fn repositories(&self) -> Result<Vec<Repository>, Error> {
        self.run(|rt| Ok(rt.state.repositories.values().cloned().collect()))
            .await
    }
    pub async fn list(&self) -> Result<Vec<Entry>, Error> {
        self.run(|rt| Ok(rt.state.workspaces.values().cloned().collect()))
            .await
    }
    pub async fn node(&self) -> Result<NodeId, Error> {
        self.run(|rt| Ok(rt.state.node.clone())).await
    }
    pub async fn operation(&self, id: OperationId) -> Result<serde_json::Value, Error> {
        self.run(move |rt| {
            let operation = rt.state.operations.get(id.as_str()).ok_or(Error::Unknown)?;
            let uses: Vec<_> = rt.state.uses.keys().filter(|session| rt.holds(session, &operation.workspace)).cloned().collect();
            let config = rt.store.state_directory().join("worktrunk-config").join(format!("{id}.toml"));
            Ok(serde_json::json!({"operation":id,"request":operation.request,"workspace":operation.workspace,"commit":operation.commit,"path":operation.path,"effectsStarted":operation.effects_started,"sessions":uses,"worktrunkConfig":config.is_file().then_some(config)}))
        }).await
    }
    pub async fn fetch(&self, id: RepositoryId) -> Result<Repository, Error> {
        self.run(move |rt| {
            let mut repo = rt
                .state
                .repositories
                .get(id.as_str())
                .cloned()
                .ok_or(Error::Unknown)?;
            rt.fetch(&mut repo)?;
            Ok(repo)
        })
        .await
    }
    pub async fn create(
        &self,
        request: CreateRequest,
        definition: ProjectDefinition,
        worktrunk: WorktrunkConfig,
        session: Option<String>,
    ) -> Result<Entry, Error> {
        self.run(move |rt| {
            let entry = rt.create(request, definition, worktrunk)?;
            if let Some(session) = session {
                rt.reserve_use(session, entry.workspace.id().clone())?;
            }
            Ok(entry)
        })
        .await
    }
    pub async fn inspect(&self, id: WorkspaceId) -> Result<Inspection, Error> {
        self.run(move |rt| rt.inspect(&id)).await
    }
    /// Explicit reconciliation never repeats hooks or switches backend/resource identity.
    pub async fn reconcile(
        &self,
        id: WorkspaceId,
        directory: Option<PathBuf>,
        hooks_reconciled: bool,
    ) -> Result<Entry, Error> {
        self.run(move |rt| rt.reconcile(id, directory, hooks_reconciled))
            .await
    }
    pub async fn attach(&self, id: WorkspaceId, session: String) -> Result<Entry, Error> {
        self.run(move |rt| {
            if !matches!(rt.inspect(&id)?, Inspection::Present { .. }) {
                return Err(Error::Conflict);
            }
            let entry = rt
                .state
                .workspaces
                .get(id.as_str())
                .cloned()
                .ok_or(Error::Unknown)?;
            if entry.workspace.state() != WorkspaceState::Ready {
                return Err(Error::Conflict);
            }
            rt.reserve_use(session, id)?;
            Ok(entry)
        })
        .await
    }
    pub async fn release(&self, session: String) -> Result<(), Error> {
        self.run(move |rt| {
            rt.state.uses.remove(&session);
            rt.state.protected.remove(&session);
            rt.state.dispatched.remove(&session);
            rt.store
                .close_resource_session(&session, &rt.state)
                .map_err(Into::into)
        })
        .await
    }
    pub(crate) async fn mark_dispatched(&self, session: String) -> Result<(), Error> {
        self.run(move |rt| {
            if rt.state.uses.contains_key(&session) {
                rt.state.dispatched.insert(session);
                rt.save()?;
            }
            Ok(())
        })
        .await
    }
    pub async fn attached(&self, session: String) -> Result<Option<Entry>, Error> {
        self.run(move |rt| {
            let Some(id) = rt.state.uses.get(&session).cloned() else {
                return Ok(None);
            };
            if !matches!(rt.inspect(&id)?, Inspection::Present { .. }) {
                return Err(Error::Conflict);
            }
            if let Some(protected) = rt.state.protected.get(&session) {
                for id in protected {
                    if !matches!(rt.inspect(id)?, Inspection::Present { .. }) {
                        return Err(Error::Conflict);
                    }
                }
            }
            Ok(rt.state.workspaces.get(id.as_str()).cloned())
        })
        .await
    }
    pub async fn track_directory(
        &self,
        path: PathBuf,
        session: String,
    ) -> Result<Option<hive_protocol::workspace::Context>, Error> {
        self.run(move |rt| {
            let requested = path.clone();
            // An unmanaged cwd may belong to a remote provider, not this node's filesystem.
            let Ok(path) = path.canonicalize() else {
                return Ok(None);
            };
            let selected = rt
                .state
                .workspaces
                .values()
                .filter(|entry| entry.workspace.state() != WorkspaceState::Removed)
                .filter(|entry| {
                    entry
                        .workspace
                        .location()
                        .is_some_and(|location| path.starts_with(location.root.as_str()))
                })
                .max_by_key(|entry| {
                    (
                        entry.workspace.intent().ownership() == Ownership::HiveManaged,
                        entry
                            .workspace
                            .location()
                            .map_or(0, |location| location.root.as_str().len()),
                    )
                })
                .cloned();
            if let Some(entry) = selected {
                rt.reserve_use(session, entry.workspace.id().clone())?;
                let context = hive_protocol::workspace::Context {
                    project: entry.workspace.intent().project.to_string(),
                    workspace: entry.workspace.id().to_string(),
                    // Keep legacy ACP path spelling consistent with the downstream scope;
                    // canonicalization above is for ownership/containment, not relocation.
                    effective_cwd: requested.to_str().ok_or(Error::Request)?.into(),
                };
                return Ok(Some(context));
            }
            Ok(None)
        })
        .await
    }
    pub async fn remove(
        &self,
        id: WorkspaceId,
        operation: OperationId,
        discard: bool,
    ) -> Result<Entry, Error> {
        self.run(move |rt| rt.remove(id, operation, discard, None))
            .await
    }
}

impl From<crate::persistence::StoreError> for Error {
    fn from(_: crate::persistence::StoreError) -> Self {
        Self::Storage
    }
}

struct Runtime<'a> {
    store: &'a Store,
    state: State,
}

/// Core backend dispatch operates on intent already admitted and durably registered by the service.
impl hive_core::workspace::backend::WorkspaceBackend for Service {
    fn provision(
        &self,
        request: hive_core::workspace::backend::ProvisionRequest,
    ) -> hive_core::workspace::backend::BackendFuture<
        '_,
        hive_core::workspace::backend::ProvisionResult,
    > {
        Box::pin(async move {
            let workspace = request.workspace.clone();
            let operation = request.operation.clone();
            self.run(move |rt| {
                request.intent.check_node(&rt.state.node)?;
                let entry = rt
                    .state
                    .workspaces
                    .get(request.workspace.as_str())
                    .cloned()
                    .ok_or(Error::Unknown)?;
                if entry.workspace.intent() != &request.intent
                    || entry.workspace.backend() != Some(&request.backend)
                    || entry.workspace.operation() != Some(&request.operation)
                {
                    return Err(Error::RetryConflict);
                }
                let original = rt
                    .state
                    .operations
                    .get(request.operation.as_str())
                    .ok_or(Error::RetryConflict)?;
                let create: CreateRequest = serde_json::from_value(original.request.clone())
                    .map_err(|_| Error::RetryConflict)?;
                let source = entry
                    .repository
                    .as_ref()
                    .and_then(|id| rt.state.repositories.get(id.as_str()))
                    .map(|repo| repo.source.clone());
                let already_ready = entry.workspace.state() == WorkspaceState::Ready;
                let result = rt.create(
                    create,
                    ProjectDefinition {
                        name: "Registered workspace".into(),
                        repository_source: source,
                        default_ref: None,
                        preparation: vec![],
                    },
                    WorktrunkConfig {
                        enabled: Some(request.backend.0 == "worktrunk"),
                    },
                )?;
                Ok(hive_core::workspace::backend::ProvisionResult {
                    workspace: request.workspace,
                    operation: request.operation,
                    location: result
                        .workspace
                        .location()
                        .cloned()
                        .ok_or(Error::Conflict)?,
                    outcome: if already_ready {
                        hive_core::workspace::backend::ProvisionOutcome::AlreadySatisfied
                    } else {
                        hive_core::workspace::backend::ProvisionOutcome::Created
                    },
                })
            })
            .await
            .map_err(|error| backend_error(error, workspace, operation, false))
        })
    }

    fn inspect(
        &self,
        target: ResourceTarget,
    ) -> hive_core::workspace::backend::BackendFuture<'_, Inspection> {
        Box::pin(async move {
            self.run(move |rt| {
                target.check_node(&rt.state.node)?;
                let entry = rt
                    .state
                    .workspaces
                    .get(target.workspace.as_str())
                    .ok_or(Error::Unknown)?;
                if entry.workspace.backend() != Some(&target.backend)
                    || entry.workspace.intent().ownership() != target.ownership
                {
                    return Ok(Inspection::IdentityConflict);
                }
                rt.inspect(&target.workspace)
            })
            .await
            .map_err(|error| match error {
                Error::Domain(error) => error,
                _ => hive_core::workspace::WorkspaceError::InvalidRecord,
            })
        })
    }

    fn cleanup(
        &self,
        request: CleanupRequest,
    ) -> hive_core::workspace::backend::BackendFuture<
        '_,
        hive_core::workspace::backend::CleanupOutcome,
    > {
        Box::pin(async move {
            let workspace = request.target.workspace.clone();
            let operation = request.operation.clone();
            self.run(move |rt| {
                request.target.check_node(&rt.state.node)?;
                let entry = rt
                    .state
                    .workspaces
                    .get(request.target.workspace.as_str())
                    .ok_or(Error::Unknown)?;
                if entry.workspace.backend() != Some(&request.target.backend)
                    || entry.workspace.intent().ownership() != request.target.ownership
                {
                    return Err(Error::Conflict);
                }
                let absent = entry.workspace.state() == WorkspaceState::Removed;
                rt.remove(
                    request.target.workspace.clone(),
                    request.operation.clone(),
                    request.mode == CleanupMode::DiscardChanges,
                    Some(request),
                )?;
                Ok(if absent {
                    hive_core::workspace::backend::CleanupOutcome::AlreadyAbsent
                } else {
                    hive_core::workspace::backend::CleanupOutcome::Removed
                })
            })
            .await
            .map_err(|error| backend_error(error, workspace, operation, true))
        })
    }
}

fn backend_error(
    error: Error,
    workspace: WorkspaceId,
    operation: OperationId,
    cleanup: bool,
) -> hive_core::workspace::WorkspaceError {
    use hive_core::workspace::{
        WorkspaceError,
        backend::{CleanupFailure, ProvisionFailure},
    };
    match error {
        Error::Domain(error) => error,
        Error::RetryConflict => WorkspaceError::RetryConflict { operation },
        Error::Uncertain => WorkspaceError::UncertainOperation { operation },
        _ if cleanup => WorkspaceError::Cleanup {
            workspace,
            reason: CleanupFailure::RemovalFailed,
            retry: RetryDisposition::InspectFirst,
        },
        Error::Conflict => WorkspaceError::Provisioning {
            workspace,
            reason: ProvisionFailure::BranchConflict,
            retry: RetryDisposition::ManualIntervention,
        },
        _ => WorkspaceError::Provisioning {
            workspace,
            reason: ProvisionFailure::BackendUnavailable,
            retry: RetryDisposition::InspectFirst,
        },
    }
}

impl Runtime<'_> {
    fn holds(&self, session: &str, id: &WorkspaceId) -> bool {
        self.state.uses.get(session) == Some(id)
            || self
                .state
                .protected
                .get(session)
                .is_some_and(|ids| ids.contains(id))
    }

    fn has_use(&self, id: &WorkspaceId) -> bool {
        self.state
            .uses
            .keys()
            .any(|session| self.holds(session, id))
    }

    fn reserve_use(&mut self, session: String, primary: WorkspaceId) -> Result<(), Error> {
        let selected = self
            .state
            .workspaces
            .get(primary.as_str())
            .ok_or(Error::Unknown)?;
        if selected.workspace.state() != WorkspaceState::Ready {
            return Err(Error::Conflict);
        }
        let directory = selected
            .workspace
            .location()
            .ok_or(Error::Conflict)?
            .working_directory
            .as_str();
        let mut protected = BTreeSet::from([primary.clone()]);
        for entry in self.state.workspaces.values() {
            if entry.workspace.intent().ownership() == Ownership::HiveManaged
                && entry.workspace.state() != WorkspaceState::Removed
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
                                operation.workspace == *entry.workspace.id()
                                    && operation.request.get("project").is_some()
                            })
                            .and_then(|operation| operation.path.clone())
                    });
                if root.is_some_and(|root| Path::new(directory).starts_with(root)) {
                    protected.insert(entry.workspace.id().clone());
                }
            }
        }
        // Validate every deletion capability affected by this directory, under the same gate.
        for id in &protected {
            let entry = self
                .state
                .workspaces
                .get(id.as_str())
                .ok_or(Error::Unknown)?;
            if entry.workspace.state() != WorkspaceState::Ready
                || !matches!(self.inspect(id)?, Inspection::Present { .. })
            {
                return Err(Error::Conflict);
            }
        }
        self.state.uses.insert(session.clone(), primary);
        self.state.protected.insert(session, protected);
        self.save()
    }
    fn reclaim_unstarted(&mut self, broker: &crate::Broker) -> Result<(), Error> {
        let sessions = self.store.records()?;
        // Inspect admission while holding the resource gate: a stale snapshot could release
        // a newly admitted startup reservation before it has saved its session row.
        let registry = broker.registry.lock().map_err(|_| Error::Storage)?;
        self.state.uses.retain(|session, _| {
            self.state.dispatched.contains(session)
                || sessions.iter().any(|record| &record.id == session)
                || registry.bindings.contains_key(session)
        });
        self.state
            .protected
            .retain(|session, _| self.state.uses.contains_key(session));
        self.save()
    }
    fn save(&self) -> Result<(), Error> {
        self.store
            .save_resource("node", &self.state)
            .map_err(Into::into)
    }
    fn path(&self, path: &Path) -> Result<NodePath, Error> {
        NodePath::new(
            self.state.node.clone(),
            if cfg!(windows) {
                PathSyntax::Windows
            } else {
                PathSyntax::Posix
            },
            path.to_str().ok_or(Error::Request)?.into(),
        )
        .map_err(Into::into)
    }
    fn root(&self) -> PathBuf {
        self.store.state_directory().join("repositories")
    }
    fn verify_repository(&self, repo: &Repository) -> Result<(), Error> {
        if repo.repository.node() != &self.state.node {
            return Err(Error::Conflict);
        }
        let root = Path::new(repo.repository.root.as_str());
        if root
            .symlink_metadata()
            .map_err(|_| Error::Conflict)?
            .file_type()
            .is_symlink()
        {
            return Err(Error::Conflict);
        }
        let actual = git(root, &["rev-parse", "--absolute-git-dir"], "inspect")?;
        if Path::new(actual.trim())
            .canonicalize()
            .map_err(|_| Error::Conflict)?
            != root.canonicalize().map_err(|_| Error::Conflict)?
        {
            return Err(Error::Conflict);
        }
        let source = git(root, &["config", "--get", "remote.origin.url"], "inspect")?;
        let identity = git(root, &["config", "--get", "hive.repository"], "inspect")?;
        if source.trim() != repo.source
            || identity.trim() != repo.repository.id.as_str()
            || git(root, &["rev-parse", "--is-bare-repository"], "inspect")?.trim() != "true"
        {
            return Err(Error::Conflict);
        }
        Ok(())
    }
    fn fetch(&mut self, repo: &mut Repository) -> Result<(), Error> {
        self.verify_repository(repo)?;
        repo.status = "fetching".into();
        repo.failure = None;
        self.state
            .repositories
            .insert(repo.repository.id.to_string(), repo.clone());
        self.save()?;
        let result = git(
            Path::new(repo.repository.root.as_str()),
            &[
                "fetch",
                "--atomic",
                "--no-tags",
                "origin",
                "+refs/heads/*:refs/remotes/origin/*",
                "refs/tags/*:refs/tags/*",
            ],
            "fetch",
        )
        .and_then(|_| refresh_remote_head(Path::new(repo.repository.root.as_str()), "fetch"));
        repo.status = if result.is_ok() { "ready" } else { "failed" }.into();
        repo.failure = result.as_ref().err().map(|e| e.code().into());
        self.state
            .repositories
            .insert(repo.repository.id.to_string(), repo.clone());
        self.save()?;
        result.map(|_| ())
    }
    fn repository(
        &mut self,
        project: &ProjectId,
        definition: &ProjectDefinition,
    ) -> Result<Repository, Error> {
        definition.validate().map_err(|_| Error::Request)?;
        let source = definition
            .repository_source
            .as_ref()
            .ok_or(Error::Request)?;
        if let Some(repo) = self
            .state
            .repositories
            .values()
            .find(|r| &r.repository.project == project && &r.source == source)
            .cloned()
        {
            return Ok(repo);
        }
        let id = RepositoryId::new();
        // Project slugs are unbounded portable identifiers, not filesystem components.
        // Stable resource IDs avoid inflating Windows linked-worktree metadata paths.
        let project_root = self.root().join(id.as_str());
        let repo = Repository {
            repository: NodeRepository {
                id,
                project: project.clone(),
                root: self.path(&project_root.join(".git"))?,
                role: RepositoryRole::Cache,
                ownership: Ownership::HiveManaged,
            },
            source: source.clone(),
            status: "cloning".into(),
            failure: None,
        };
        self.state
            .repositories
            .insert(repo.repository.id.to_string(), repo.clone());
        // Registration is inert; publish it atomically with the initial workspace and operation.
        Ok(repo)
    }
    fn acquire(&mut self, mut repo: Repository) -> Result<Repository, Error> {
        let destination = Path::new(repo.repository.root.as_str());
        let parent = destination.parent().ok_or(Error::Request)?;
        std::fs::create_dir_all(parent).map_err(|_| Error::Storage)?;
        let staging = parent.join(".git-incomplete");
        // A partial acquisition is owned by the durable repository ID; never remove arbitrary occupied paths.
        let result = (|| {
            if !staging.exists() {
                git(
                    parent,
                    &["init", "--bare", staging.to_str().ok_or(Error::Request)?],
                    "clone",
                )?;
            }
            git(
                &staging,
                &["config", "extensions.worktreeConfig", "true"],
                "clone",
            )?;
            #[cfg(windows)]
            git(&staging, &["config", "core.longpaths", "true"], "clone")?;
            git(
                &staging,
                &["config", "--worktree", "core.bare", "true"],
                "clone",
            )?;
            // With worktreeConfig enabled, bare=true must not leak into linked worktrees.
            let _ = git(
                &staging,
                &["config", "--local", "--unset-all", "core.bare"],
                "clone",
            );
            // Templates can contribute checkout-specific config; the extension removes
            // Git's old main-worktree exception, so these settings must not remain shared.
            for name in [
                "core.worktree",
                "core.sparseCheckout",
                "core.sparseCheckoutCone",
            ] {
                if let Ok(value) = git(&staging, &["config", "--local", "--get", name], "clone") {
                    git(
                        &staging,
                        &["config", "--worktree", name, value.trim()],
                        "clone",
                    )?;
                    git(
                        &staging,
                        &["config", "--local", "--unset-all", name],
                        "clone",
                    )?;
                }
            }
            let common = git(
                &staging,
                &["config", "--local", "--list", "--null"],
                "clone",
            )?;
            if common.split('\0').any(|entry| {
                entry.starts_with("core.bare\n") || entry.starts_with("core.worktree\n")
            }) {
                return Err(Error::Git("clone"));
            }
            git(
                &staging,
                &["config", "remote.origin.url", &repo.source],
                "clone",
            )?;
            git(
                &staging,
                &["config", "hive.repository", repo.repository.id.as_str()],
                "clone",
            )?;
            git(
                &staging,
                &[
                    "config",
                    "remote.origin.fetch",
                    "+refs/heads/*:refs/remotes/origin/*",
                ],
                "clone",
            )?;
            git(
                &staging,
                &[
                    "fetch",
                    "--no-tags",
                    "origin",
                    "+refs/heads/*:refs/remotes/origin/*",
                    "refs/tags/*:refs/tags/*",
                ],
                "clone",
            )?;
            let head = git(
                &staging,
                &["ls-remote", "--symref", "origin", "HEAD"],
                "clone",
            )?;
            if let Some(branch) = head.lines().find_map(|line| {
                line.strip_prefix("ref: refs/heads/")
                    .and_then(|s| s.split_once('\t'))
                    .map(|(branch, _)| branch)
            }) {
                git(
                    &staging,
                    &[
                        "symbolic-ref",
                        "refs/remotes/origin/HEAD",
                        &format!("refs/remotes/origin/{branch}"),
                    ],
                    "clone",
                )?;
                // Seed only a fresh repository's default branch/HEAD. Later fetches update
                // remote-tracking refs exclusively, preserving any checked-out local branch.
                git(
                    &staging,
                    &[
                        "update-ref",
                        &format!("refs/heads/{branch}"),
                        &format!("refs/remotes/origin/{branch}"),
                    ],
                    "clone",
                )?;
                git(
                    &staging,
                    &["symbolic-ref", "HEAD", &format!("refs/heads/{branch}")],
                    "clone",
                )?;
            }
            std::fs::rename(&staging, destination).map_err(|_| Error::Storage)?;
            Ok::<_, Error>(())
        })();
        repo.status = if result.is_ok() { "ready" } else { "failed" }.into();
        repo.failure = result.as_ref().err().map(|e| e.code().into());
        self.state
            .repositories
            .insert(repo.repository.id.to_string(), repo.clone());
        self.save()?;
        result?;
        Ok(repo)
    }

    fn create(
        &mut self,
        request: CreateRequest,
        definition: ProjectDefinition,
        worktrunk: WorktrunkConfig,
    ) -> Result<Entry, Error> {
        request.policy.validate().map_err(|_| Error::Request)?;
        let key = request.operation.to_string();
        let json = serde_json::to_value(&request).map_err(|_| Error::Request)?;
        if let Some(original) = self.state.operations.get(&key).cloned() {
            if original.request != json {
                return Err(Error::RetryConflict);
            }
            let mut entry = self
                .state
                .workspaces
                .get(original.workspace.as_str())
                .cloned()
                .ok_or(Error::Conflict)?;
            if !original.effects_started && entry.workspace.state() == WorkspaceState::Provisioning
            {
                // No checkout mutation was dispatched; interrupted cache work can be retried
                // under the same repository guard without creating another workspace identity.
                entry
                    .workspace
                    .transition(WorkspaceEvent::ProvisionFailed {
                        retry: RetryDisposition::InspectFirst,
                        location: None,
                    })?;
                self.state
                    .workspaces
                    .insert(original.workspace.to_string(), entry.clone());
                self.save()?;
            }
            if !original.effects_started && entry.workspace.state() == WorkspaceState::Failed {
                entry
                    .workspace
                    .transition(WorkspaceEvent::ReconciledRetry)?;
                entry.workspace.transition(WorkspaceEvent::Provision {
                    operation: request.operation.clone(),
                    backend: entry.workspace.backend().cloned().ok_or(Error::Conflict)?,
                })?;
                self.state
                    .workspaces
                    .insert(original.workspace.to_string(), entry.clone());
                self.save()?;
                let intent = match &entry.workspace.intent().policy {
                    WorkspacePolicy::Worktree { git, .. } | WorkspacePolicy::Clone { git } => {
                        Some(git.clone())
                    }
                    WorkspacePolicy::Ephemeral {
                        backing:
                            ManagedBacking::Worktree { git, .. } | ManagedBacking::Clone { git },
                    } => Some(git.clone()),
                    _ => None,
                };
                match self.provision(&entry, intent.as_ref(), &key) {
                    Ok(location) => {
                        entry.failure = None;
                        entry
                            .workspace
                            .transition(WorkspaceEvent::Provisioned { location })?;
                    }
                    Err(error) => {
                        entry.failure = Some(error.code().into());
                        entry
                            .workspace
                            .transition(WorkspaceEvent::ProvisionFailed {
                                retry: RetryDisposition::InspectFirst,
                                location: None,
                            })?;
                        self.state
                            .workspaces
                            .insert(original.workspace.to_string(), entry);
                        self.save()?;
                        return Err(error);
                    }
                }
                self.state
                    .workspaces
                    .insert(original.workspace.to_string(), entry.clone());
                self.save()?;
                return Ok(entry);
            }
            match self.inspect(&original.workspace)? {
                Inspection::Present { .. } if entry.workspace.state() == WorkspaceState::Ready => {
                    return Ok(entry);
                }
                Inspection::Present { location, .. }
                    if original.effects_started
                        && entry
                            .workspace
                            .backend()
                            .is_some_and(|b| b.0 != "worktrunk") =>
                {
                    let mut entry = entry;
                    entry.failure = None;
                    if let Some(base) = original.commit.as_ref()
                        && original.commit != location.commit
                    {
                        git(
                            Path::new(location.root.as_str()),
                            &["merge-base", "--is-ancestor", base, "HEAD"],
                            "base verification",
                        )
                        .map_err(|_| Error::Conflict)?;
                    }
                    entry
                        .workspace
                        .transition(WorkspaceEvent::ReconciledReady { location })?;
                    self.state
                        .workspaces
                        .insert(original.workspace.to_string(), entry.clone());
                    self.save()?;
                    return Ok(entry);
                }
                _ => return Err(Error::Uncertain),
            }
        }
        // Reject inert invalid input before reserving an operation with no resource to reconcile.
        let existing_directory = if let WorkspaceTemplate::Existing { directory } = &request.policy
        {
            if request.branch.is_some() || request.base_ref.is_some() {
                return Err(Error::Request);
            }
            let directory = directory
                .clone()
                .or(self
                    .store
                    .project_override(&request.project, &self.state.node)?
                    .map(|path| PathBuf::from(path.as_str())))
                .ok_or(Error::Request)?;
            if !directory.is_absolute() {
                return Err(Error::Request);
            }
            Some(self.path(&directory.canonicalize().map_err(|_| Error::Request)?)?)
        } else {
            definition.validate().map_err(|_| Error::Request)?;
            if definition.repository_source.is_none() {
                return Err(Error::Request);
            }
            None
        };
        let id = WorkspaceId::new();
        let mut repository = None;
        let mut git_intent = None;
        let retention = match &request.policy {
            WorkspaceTemplate::Ephemeral { retention, .. } => retention.clone(),
            _ => Retention::Retain,
        };
        let existing = matches!(request.policy, WorkspaceTemplate::Existing { .. });
        let clone = matches!(&request.policy, WorkspaceTemplate::Clone { .. })
            || matches!(&request.policy, WorkspaceTemplate::Ephemeral { backing, .. } if backing == "clone");
        let backend = if existing {
            "existing"
        } else if clone {
            "clone"
        } else {
            match worktrunk.select(probe_worktrunk)? {
                IntegrationSelection::Native => "native",
                IntegrationSelection::Worktrunk => "worktrunk",
            }
        };
        // Build one initial snapshot: an operation must never durably point at a missing workspace.
        self.state.operations.insert(
            key.clone(),
            Operation {
                request: json,
                workspace: id.clone(),
                commit: None,
                path: None,
                effects_started: false,
            },
        );
        let policy = if let Some(directory) = existing_directory {
            WorkspacePolicy::Existing { directory }
        } else {
            let repo = self.repository(&request.project, &definition)?;
            let configured_base = match &request.policy {
                WorkspaceTemplate::Worktree { base_ref }
                | WorkspaceTemplate::Clone { base_ref }
                | WorkspaceTemplate::Ephemeral { base_ref, .. } => base_ref.clone(),
                _ => None,
            };
            let base = request
                .base_ref
                .clone()
                .or(configured_base)
                .or(definition.default_ref)
                .unwrap_or(GitRef::try_from("refs/remotes/origin/HEAD".to_string())?);
            let git = GitIntent {
                base_ref: base,
                branch: request.branch.clone(),
            };
            repository = Some(repo.repository.id.clone());
            git_intent = Some(git.clone());
            let backing = if clone {
                ManagedBacking::Clone { git }
            } else {
                ManagedBacking::Worktree {
                    repository: repo.repository.id,
                    git,
                }
            };
            if matches!(request.policy, WorkspaceTemplate::Ephemeral { .. }) {
                WorkspacePolicy::Ephemeral { backing }
            } else {
                match backing {
                    ManagedBacking::Clone { git } => WorkspacePolicy::Clone { git },
                    ManagedBacking::Worktree { repository, git } => {
                        WorkspacePolicy::Worktree { repository, git }
                    }
                }
            }
        };
        let mut workspace = Workspace::new(
            id.clone(),
            WorkspaceIntent {
                project: request.project,
                node: self.state.node.clone(),
                policy,
                retention,
            },
        )?;
        workspace.transition(WorkspaceEvent::Provision {
            operation: request.operation,
            backend: BackendId(backend.into()),
        })?;
        let mut entry = Entry {
            workspace,
            repository,
            failure: None,
            archive: None,
        };
        self.state.workspaces.insert(id.to_string(), entry.clone());
        self.save()?;
        let result = self.provision(&entry, git_intent.as_ref(), &key);
        match result {
            Ok(location) => entry
                .workspace
                .transition(WorkspaceEvent::Provisioned { location })?,
            Err(error) => {
                entry.failure = Some(error.code().into());
                entry
                    .workspace
                    .transition(WorkspaceEvent::ProvisionFailed {
                        retry: RetryDisposition::InspectFirst,
                        location: None,
                    })?;
                self.state.workspaces.insert(id.to_string(), entry);
                self.save()?;
                return Err(error);
            }
        }
        self.state.workspaces.insert(id.to_string(), entry.clone());
        self.save()?;
        Ok(entry)
    }

    fn provision(
        &mut self,
        entry: &Entry,
        intent: Option<&GitIntent>,
        key: &str,
    ) -> Result<WorkspaceLocation, Error> {
        let backend = entry.workspace.backend().ok_or(Error::Conflict)?.clone();
        if let WorkspacePolicy::Existing { directory } = &entry.workspace.intent().policy {
            self.state
                .operations
                .get_mut(key)
                .ok_or(Error::Conflict)?
                .path = Some(PathBuf::from(directory.as_str()));
            self.save()?;
            return Ok(WorkspaceLocation {
                root: directory.clone(),
                working_directory: directory.clone(),
                backend,
                resolved_base: None,
                branch: None,
                commit: None,
            });
        }
        let git_intent = intent.ok_or(Error::Request)?;
        let branch = git_intent.effective_branch(entry.workspace.id());
        let mut repo = self
            .state
            .repositories
            .get(entry.repository.as_ref().ok_or(Error::Conflict)?.as_str())
            .ok_or(Error::Conflict)?
            .clone();
        if Path::new(repo.repository.root.as_str()).exists() {
            self.fetch(&mut repo)?;
        } else {
            repo = self.acquire(repo)?;
        }
        let bare = Path::new(repo.repository.root.as_str());
        if self
            .state
            .operations
            .get(key)
            .and_then(|o| o.commit.as_ref())
            .is_none()
        {
            let resolved = resolve(bare, &git_intent.base_ref)?;
            self.state
                .operations
                .get_mut(key)
                .ok_or(Error::Conflict)?
                .commit = Some(resolved);
        }
        self.save()?;
        git(
            bare,
            &["check-ref-format", "--branch", &branch],
            "branch validation",
        )?;
        if git(
            bare,
            &["show-ref", "--verify", &format!("refs/heads/{branch}")],
            "inspect",
        )
        .is_ok()
        {
            return Err(Error::Conflict);
        }
        let commit = self
            .state
            .operations
            .get(key)
            .and_then(|o| o.commit.clone())
            .ok_or(Error::Conflict)?;
        let target = if backend.0 == "clone" {
            self.store
                .state_directory()
                .join("clones")
                .join(entry.workspace.id().as_str())
        } else {
            bare.parent()
                .ok_or(Error::Conflict)?
                .join(entry.workspace.id().as_str())
        };
        if backend.0 != "worktrunk" && target.try_exists().map_err(|_| Error::Conflict)? {
            return Err(Error::Conflict);
        }
        let operation = self.state.operations.get_mut(key).ok_or(Error::Conflict)?;
        operation.path = (backend.0 != "worktrunk").then_some(target.clone());
        operation.effects_started = backend.0 != "worktrunk";
        self.save()?;
        let actual = if backend.0 == "worktrunk" {
            // Bare repositories have no checkout from which wt can read branch-specific hooks.
            let configured = std::env::var_os("WORKTRUNK_PROJECT_CONFIG_PATH")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(".config/wt.toml"));
            let project_config = if configured.is_absolute() {
                Some(configured)
            } else {
                match git(
                    bare,
                    &[
                        "show",
                        &format!("{commit}:{}", configured.to_str().ok_or(Error::Request)?),
                    ],
                    "hook configuration",
                ) {
                    Ok(contents) => {
                        let directory = self.store.state_directory().join("worktrunk-config");
                        std::fs::create_dir_all(&directory).map_err(|_| Error::Storage)?;
                        let path = directory.join(format!("{key}.toml"));
                        std::fs::write(&path, contents).map_err(|_| Error::Storage)?;
                        Some(path)
                    }
                    Err(Error::Git(_)) => None,
                    Err(error) => return Err(error),
                }
            };
            check_approvals_with_config(bare, project_config.as_deref())?;
            self.state
                .operations
                .get_mut(key)
                .ok_or(Error::Conflict)?
                .effects_started = true;
            self.save()?;
            let output = wt_with_config(
                bare,
                &[
                    "switch", "--create", "--base", &commit, "--no-cd", "--format", "json", &branch,
                ],
                project_config.as_deref(),
            )?;
            let value: serde_json::Value =
                serde_json::from_str(&output).map_err(|_| Error::Uncertain)?;
            if value["action"].as_str() != Some("created")
                || value["created_branch"] != true
                || value["branch"].as_str() != Some(branch.as_str())
            {
                return Err(Error::Uncertain);
            }
            PathBuf::from(
                value
                    .get("path")
                    .and_then(|p| p.as_str())
                    .ok_or(Error::Uncertain)?,
            )
        } else if backend.0 == "clone" {
            std::fs::create_dir_all(target.parent().ok_or(Error::Request)?)
                .map_err(|_| Error::Storage)?;
            git(
                bare,
                &[
                    "clone",
                    "--no-hardlinks",
                    "--no-checkout",
                    bare.to_str().ok_or(Error::Request)?,
                    target.to_str().ok_or(Error::Request)?,
                ],
                "clone",
            )?;
            git(
                &target,
                &["remote", "set-url", "origin", &repo.source],
                "clone",
            )?;
            #[cfg(windows)]
            git(&target, &["config", "core.longpaths", "true"], "clone")?;
            git(&target, &["checkout", "-b", &branch, &commit], "checkout")?;
            target
        } else {
            git(
                bare,
                &[
                    "worktree",
                    "add",
                    "-b",
                    &branch,
                    target.to_str().ok_or(Error::Request)?,
                    &commit,
                ],
                "worktree creation",
            )?;
            target
        };
        if !actual.is_absolute() {
            return Err(Error::Uncertain);
        }
        let actual = actual.canonicalize().map_err(|_| Error::Conflict)?;
        // The branch/path alone cannot distinguish our checkout from a replacement repository.
        git(
            &actual,
            &[
                "config",
                if backend.0 == "clone" {
                    "--local"
                } else {
                    "--worktree"
                },
                "hive.workspace",
                entry.workspace.id().as_str(),
            ],
            "ownership",
        )?;
        self.state
            .operations
            .get_mut(key)
            .ok_or(Error::Conflict)?
            .path = Some(actual.clone());
        self.save()?;
        self.location(entry, &actual, Some(&commit))
    }

    fn location(
        &self,
        entry: &Entry,
        path: &Path,
        expected: Option<&str>,
    ) -> Result<WorkspaceLocation, Error> {
        let backend = entry.workspace.backend().ok_or(Error::Conflict)?.clone();
        let root = self.path(path)?;
        if backend.0 == "existing" {
            if !path.is_dir() {
                return Err(Error::Conflict);
            }
            return Ok(WorkspaceLocation {
                root: root.clone(),
                working_directory: root,
                backend,
                resolved_base: None,
                branch: None,
                commit: None,
            });
        }
        let top = git(path, &["rev-parse", "--show-toplevel"], "inspect")?;
        let ownership = git(path, &["config", "--get", "hive.workspace"], "inspect")?;
        if ownership.trim() != entry.workspace.id().as_str() {
            return Err(Error::Conflict);
        }
        if Path::new(top.trim())
            .canonicalize()
            .map_err(|_| Error::Conflict)?
            != path
        {
            return Err(Error::Conflict);
        }
        if backend.0 != "clone" {
            let common = git(
                path,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
                "inspect",
            )?;
            let repo = self
                .state
                .repositories
                .get(entry.repository.as_ref().ok_or(Error::Conflict)?.as_str())
                .ok_or(Error::Conflict)?;
            self.verify_repository(repo)?;
            if Path::new(common.trim())
                .canonicalize()
                .map_err(|_| Error::Conflict)?
                != Path::new(repo.repository.root.as_str())
                    .canonicalize()
                    .map_err(|_| Error::Conflict)?
            {
                return Err(Error::Conflict);
            }
        }
        let branch = git(path, &["symbolic-ref", "--short", "HEAD"], "inspect")?;
        let git_intent = match &entry.workspace.intent().policy {
            WorkspacePolicy::Worktree { git, .. } | WorkspacePolicy::Clone { git } => git,
            WorkspacePolicy::Ephemeral {
                backing: ManagedBacking::Worktree { git, .. } | ManagedBacking::Clone { git },
            } => git,
            _ => return Err(Error::Conflict),
        };
        if branch.trim() != git_intent.effective_branch(entry.workspace.id()) {
            return Err(Error::Conflict);
        }
        let commit = git(path, &["rev-parse", "HEAD"], "inspect")?
            .trim()
            .to_owned();
        if let Some(expected) = expected
            && commit != expected
        {
            // Lifecycle hooks can legitimately commit setup work; retain the observed HEAD
            // while proving the task still descends from the exact requested base.
            git(
                path,
                &["merge-base", "--is-ancestor", expected, "HEAD"],
                "base verification",
            )
            .map_err(|_| Error::Conflict)?;
        }
        let pinned = self
            .state
            .operations
            .values()
            .find(|operation| {
                &operation.workspace == entry.workspace.id()
                    && operation.request.get("project").is_some()
            })
            .and_then(|operation| operation.commit.clone())
            .ok_or(Error::Conflict)?;
        Ok(WorkspaceLocation {
            root: root.clone(),
            working_directory: root,
            backend,
            resolved_base: Some(GitRef::try_from(pinned)?),
            branch: Some(GitRef::try_from(branch.trim().to_owned())?),
            commit: Some(commit),
        })
    }

    fn inspect(&self, id: &WorkspaceId) -> Result<Inspection, Error> {
        let entry = self
            .state
            .workspaces
            .get(id.as_str())
            .ok_or(Error::Unknown)?;
        if entry.workspace.intent().node != self.state.node {
            return Ok(Inspection::IdentityConflict);
        }
        let operation = self
            .state
            .operations
            .values()
            .find(|o| &o.workspace == id && o.request.get("project").is_some());
        let path = entry
            .workspace
            .location()
            .map(|l| PathBuf::from(l.root.as_str()))
            .or_else(|| operation.and_then(|o| o.path.clone()));
        let Some(path) = path else {
            return Ok(Inspection::Partial {
                location: None,
                safety: SafetyEvidence {
                    ownership_verified: false,
                    cleanliness: Cleanliness::Unknown,
                    usage: ResourceUse::Unknown,
                },
            });
        };
        if !path.try_exists().map_err(|_| Error::Conflict)? {
            return if entry.workspace.state() == WorkspaceState::Removed {
                Ok(Inspection::Absent)
            } else {
                Ok(Inspection::Partial {
                    location: None,
                    safety: SafetyEvidence {
                        ownership_verified: false,
                        cleanliness: Cleanliness::Unknown,
                        usage: ResourceUse::Unknown,
                    },
                })
            };
        }
        if path
            .symlink_metadata()
            .map_err(|_| Error::Conflict)?
            .file_type()
            .is_symlink()
        {
            return Ok(Inspection::IdentityConflict);
        }
        let location = match self.location(entry, &path, None) {
            Ok(location) => location,
            Err(_) => return Ok(Inspection::IdentityConflict),
        };
        let cleanliness = if entry.workspace.intent().ownership() == Ownership::External {
            Cleanliness::Unknown
        } else {
            match git(
                &path,
                &[
                    "status",
                    "--porcelain=v1",
                    "--untracked-files=all",
                    "--ignored",
                    "--ignore-submodules=none",
                ],
                "inspect",
            ) {
                Ok(status) if status.is_empty() => Cleanliness::Clean,
                Ok(_) => Cleanliness::Dirty,
                Err(_) => Cleanliness::Unknown,
            }
        };
        let holders: Vec<_> = self
            .state
            .uses
            .iter()
            .filter(|(session, _)| self.holds(session, id))
            .map(|(session, _)| session)
            .collect();
        let usage = if holders.is_empty() {
            ResourceUse::Idle
        } else {
            let sessions = self.store.records()?;
            if holders.iter().any(|id| {
                sessions.iter().any(|record| {
                    &record.id == *id
                        && matches!(record.outcome, crate::persistence::Outcome::Active)
                })
            }) {
                ResourceUse::Active
            } else {
                ResourceUse::Unknown
            }
        };
        Ok(Inspection::Present {
            location,
            safety: SafetyEvidence {
                ownership_verified: true,
                cleanliness,
                usage,
            },
        })
    }

    fn remove(
        &mut self,
        id: WorkspaceId,
        operation: OperationId,
        discard: bool,
        durable: Option<CleanupRequest>,
    ) -> Result<Entry, Error> {
        let mut entry = self
            .state
            .workspaces
            .get(id.as_str())
            .cloned()
            .ok_or(Error::Unknown)?;
        let cleanup = durable.unwrap_or(CleanupRequest {
            operation: operation.clone(),
            target: ResourceTarget {
                workspace: id.clone(),
                node: self.state.node.clone(),
                backend: entry.workspace.backend().cloned().ok_or(Error::Conflict)?,
                ownership: entry.workspace.intent().ownership(),
                location: entry.workspace.location().cloned(),
            },
            mode: if discard {
                CleanupMode::DiscardChanges
            } else {
                CleanupMode::PreserveChanges
            },
        });
        let request = serde_json::to_value(&cleanup).map_err(|_| Error::Request)?;
        if let Some(original) = self.state.operations.get(operation.as_str()) {
            if original.request != request {
                return Err(Error::RetryConflict);
            }
            if entry.workspace.state() == WorkspaceState::Removed {
                return Ok(entry);
            }
            if entry
                .workspace
                .backend()
                .is_some_and(|backend| backend.0 == "worktrunk")
                && original.effects_started
            {
                return Err(Error::Uncertain);
            }
            if !matches!(
                entry.workspace.state(),
                WorkspaceState::Cleaning | WorkspaceState::CleanupFailed
            ) {
                return Err(Error::Conflict);
            }
            if self.verified_removal_absence(&entry)? {
                self.finish_cleanup(&mut entry, &operation)?;
                self.state.workspaces.insert(id.to_string(), entry.clone());
                self.save()?;
                return Ok(entry);
            }
            if entry.workspace.state() == WorkspaceState::Cleaning {
                entry.workspace.transition(WorkspaceEvent::CleanupFailed {
                    retry: RetryDisposition::InspectFirst,
                })?;
            }
            entry
                .workspace
                .transition(WorkspaceEvent::ReconciledRetry)?;
        }
        let Inspection::Present { location, safety } = self.inspect(&id)? else {
            return Err(Error::Conflict);
        };
        cleanup.check_safety(&safety)?;
        if location.backend.0 == "worktrunk" {
            check_approvals(Path::new(location.root.as_str()))?;
        }
        entry.workspace.transition(WorkspaceEvent::Cleanup {
            operation: operation.clone(),
        })?;
        self.state.operations.insert(
            operation.to_string(),
            Operation {
                request,
                workspace: id.clone(),
                commit: None,
                path: Some(PathBuf::from(location.root.as_str())),
                effects_started: true,
            },
        );
        self.state.workspaces.insert(id.to_string(), entry.clone());
        self.save()?;
        let path = Path::new(location.root.as_str());
        let result = if location.backend.0 == "clone" {
            (|| {
                if !discard {
                    entry.archive = Some(self.archive_clone(&entry, &operation, path)?);
                    self.state.workspaces.insert(id.to_string(), entry.clone());
                    self.save()?;
                }
                std::fs::remove_dir_all(path).map_err(|_| Error::Storage)
            })()
        } else {
            let repo = self
                .state
                .repositories
                .get(entry.repository.as_ref().ok_or(Error::Conflict)?.as_str())
                .ok_or(Error::Conflict)?;
            let bare = Path::new(repo.repository.root.as_str());
            if location.backend.0 == "worktrunk" {
                let mut args = vec![
                    "remove",
                    "--foreground",
                    "--no-delete-branch",
                    "--format",
                    "json",
                ];
                if discard {
                    args.push("--force");
                }
                args.push(location.root.as_str());
                let config = std::env::var_os("WORKTRUNK_PROJECT_CONFIG_PATH")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from(".config/wt.toml"));
                let config = if config.is_absolute() {
                    config
                } else {
                    path.join(config)
                };
                wt_with_config(bare, &args, config.is_file().then_some(config.as_path()))
                    .map(|_| ())
            } else {
                let mut args = vec!["worktree", "remove"];
                if discard {
                    args.push("--force");
                }
                args.push(location.root.as_str());
                git(bare, &args, "worktree removal").map(|_| ())
            }
        };
        let result = result.and_then(|()| {
            if path.try_exists().map_err(|_| Error::Conflict)? {
                Err(Error::Uncertain)
            } else {
                Ok(())
            }
        });
        match &result {
            Ok(()) => {
                entry.failure = None;
                entry.workspace.transition(WorkspaceEvent::Cleaned)?;
            }
            Err(error) => {
                entry.failure = Some(error.code().into());
                entry.workspace.transition(WorkspaceEvent::CleanupFailed {
                    retry: RetryDisposition::InspectFirst,
                })?;
            }
        }
        self.state.workspaces.insert(id.to_string(), entry.clone());
        self.save()?;
        result?;
        Ok(entry)
    }

    fn archive_clone(
        &mut self,
        entry: &Entry,
        operation: &OperationId,
        source: &Path,
    ) -> Result<NodePath, Error> {
        let head = git(source, &["rev-parse", "HEAD"], "archive")?;
        let refs = git(
            source,
            &["for-each-ref", "--format=%(objectname) %(refname)"],
            "archive",
        )?;
        let directory = self.store.state_directory().join("archives");
        let destination = directory.join(format!("{}.git", entry.workspace.id()));
        let plan = if let Some(plan) = self.state.archives.get(operation.as_str()) {
            if plan.head != head || plan.refs != refs || plan.destination != destination {
                return Err(Error::Conflict);
            }
            plan.clone()
        } else {
            let staging =
                directory.join(format!(".{}-{operation}.incomplete", entry.workspace.id()));
            if destination.try_exists().map_err(|_| Error::Storage)?
                || staging.try_exists().map_err(|_| Error::Storage)?
            {
                return Err(Error::Conflict);
            }
            let plan = ArchivePlan {
                destination,
                staging,
                head,
                refs,
            };
            // Persist ownership and source refs before creating backup files, closing both
            // the completed-clone and rename-before-publication crash windows.
            self.state
                .archives
                .insert(operation.to_string(), plan.clone());
            self.save()?;
            plan
        };
        let complete = |path: &Path| -> bool {
            git(path, &["rev-parse", "HEAD"], "archive").is_ok_and(|head| head == plan.head)
                && git(
                    path,
                    &["for-each-ref", "--format=%(objectname) %(refname)"],
                    "archive",
                )
                .is_ok_and(|refs| refs == plan.refs)
                && git(path, &["fsck", "--no-reflogs"], "archive").is_ok()
        };
        if plan.destination.exists() {
            if !complete(&plan.destination)
                || git(
                    &plan.destination,
                    &["config", "--get", "hive.archiveOperation"],
                    "archive",
                )?
                .trim()
                    != operation.as_str()
            {
                return Err(Error::Conflict);
            }
        } else {
            std::fs::create_dir_all(&directory).map_err(|_| Error::Storage)?;
            if plan.staging.exists() && !complete(&plan.staging) {
                // Only an operation-owned temporary copy is retried, and only while the
                // original checkout still matches the durable source snapshot above.
                std::fs::remove_dir_all(&plan.staging).map_err(|_| Error::Storage)?;
            }
            if !plan.staging.exists() {
                git(
                    source,
                    &[
                        "clone",
                        "--mirror",
                        "--no-hardlinks",
                        source.to_str().ok_or(Error::Request)?,
                        plan.staging.to_str().ok_or(Error::Request)?,
                    ],
                    "archive",
                )?;
            }
            if !complete(&plan.staging) {
                return Err(Error::Conflict);
            }
            git(
                &plan.staging,
                &["config", "hive.archiveOperation", operation.as_str()],
                "archive",
            )?;
            #[cfg(windows)]
            git(
                &plan.staging,
                &["config", "core.longpaths", "true"],
                "archive",
            )?;
            std::fs::rename(&plan.staging, &plan.destination).map_err(|_| Error::Storage)?;
        }
        self.path(&plan.destination)
    }

    fn verified_removal_absence(&self, entry: &Entry) -> Result<bool, Error> {
        let location = entry.workspace.location().ok_or(Error::Conflict)?;
        if Path::new(location.root.as_str())
            .try_exists()
            .map_err(|_| Error::Conflict)?
        {
            return Ok(false);
        }
        if self.has_use(entry.workspace.id()) {
            return Err(Error::Conflict);
        }
        if location.backend.0 == "clone" {
            // The original discard mode or durable archive distinguishes safe completed deletion.
            let operation = self
                .state
                .operations
                .get(entry.workspace.operation().ok_or(Error::Conflict)?.as_str())
                .ok_or(Error::Conflict)?;
            let request: CleanupRequest =
                serde_json::from_value(operation.request.clone()).map_err(|_| Error::Conflict)?;
            return Ok(request.mode == CleanupMode::DiscardChanges
                || entry
                    .archive
                    .as_ref()
                    .is_some_and(|archive| Path::new(archive.as_str()).is_dir()));
        }
        let repository = self
            .state
            .repositories
            .get(entry.repository.as_ref().ok_or(Error::Conflict)?.as_str())
            .ok_or(Error::Conflict)?;
        self.verify_repository(repository)?;
        let registrations = git(
            Path::new(repository.repository.root.as_str()),
            &["worktree", "list", "--porcelain", "-z"],
            "inspect",
        )?;
        Ok(!registered_worktree(
            &registrations,
            Path::new(location.root.as_str()),
        )?)
    }

    fn reconcile(
        &mut self,
        id: WorkspaceId,
        directory: Option<PathBuf>,
        hooks_reconciled: bool,
    ) -> Result<Entry, Error> {
        let mut entry = self
            .state
            .workspaces
            .get(id.as_str())
            .cloned()
            .ok_or(Error::Unknown)?;
        if self.has_use(&id) {
            return Err(Error::Conflict);
        }
        let key = entry
            .workspace
            .operation()
            .ok_or(Error::Conflict)?
            .to_string();
        let operation = self
            .state
            .operations
            .get(&key)
            .cloned()
            .ok_or(Error::Conflict)?;
        let backend = entry.workspace.backend().ok_or(Error::Conflict)?.0.as_str();
        if backend == "worktrunk" && !hooks_reconciled {
            return Err(Error::Uncertain);
        }
        if matches!(
            entry.workspace.state(),
            WorkspaceState::Cleaning | WorkspaceState::CleanupFailed
        ) {
            if self.verified_removal_absence(&entry)? {
                self.finish_cleanup(
                    &mut entry,
                    &OperationId::try_from(key.clone()).map_err(|_| Error::Conflict)?,
                )?;
            } else {
                let Inspection::Present { .. } = self.inspect(&id)? else {
                    return Err(Error::Conflict);
                };
                if entry.workspace.state() == WorkspaceState::Cleaning {
                    entry.workspace.transition(WorkspaceEvent::CleanupFailed {
                        retry: RetryDisposition::InspectFirst,
                    })?;
                }
                entry
                    .workspace
                    .transition(WorkspaceEvent::ReconciledRetry)?;
                self.state
                    .operations
                    .get_mut(&key)
                    .ok_or(Error::Conflict)?
                    .effects_started = false;
            }
        } else if matches!(
            entry.workspace.state(),
            WorkspaceState::Provisioning | WorkspaceState::Failed
        ) {
            if !operation.effects_started {
                return Err(Error::Request);
            }
            let path = directory.or(operation.path.clone()).ok_or(Error::Request)?;
            if !path.is_absolute() {
                return Err(Error::Request);
            }
            let path = path.canonicalize().map_err(|_| Error::Conflict)?;
            if backend != "worktrunk" {
                let intended = operation
                    .path
                    .as_ref()
                    .ok_or(Error::Conflict)?
                    .canonicalize()
                    .map_err(|_| Error::Conflict)?;
                if intended != path {
                    return Err(Error::Conflict);
                }
            }
            let repository = self
                .state
                .repositories
                .get(entry.repository.as_ref().ok_or(Error::Conflict)?.as_str())
                .ok_or(Error::Conflict)?;
            self.verify_repository(repository)?;
            if backend != "clone" {
                let common = git(
                    &path,
                    &["rev-parse", "--path-format=absolute", "--git-common-dir"],
                    "inspect",
                )?;
                if Path::new(common.trim())
                    .canonicalize()
                    .map_err(|_| Error::Conflict)?
                    != Path::new(repository.repository.root.as_str())
                        .canonicalize()
                        .map_err(|_| Error::Conflict)?
                {
                    return Err(Error::Conflict);
                }
                let list = git(
                    Path::new(repository.repository.root.as_str()),
                    &["worktree", "list", "--porcelain", "-z"],
                    "inspect",
                )?;
                if !registered_worktree(&list, &path)? {
                    return Err(Error::Conflict);
                }
            } else if git(&path, &["config", "--get", "remote.origin.url"], "inspect")?.trim()
                != repository.source
            {
                return Err(Error::Conflict);
            }
            let commit = operation.commit.as_ref().ok_or(Error::Conflict)?;
            if git(&path, &["rev-parse", "HEAD"], "inspect")?.trim() != commit {
                git(
                    &path,
                    &["merge-base", "--is-ancestor", commit, "HEAD"],
                    "base verification",
                )
                .map_err(|_| Error::Conflict)?;
            }
            let intent = match &entry.workspace.intent().policy {
                WorkspacePolicy::Worktree { git, .. }
                | WorkspacePolicy::Clone { git }
                | WorkspacePolicy::Ephemeral {
                    backing: ManagedBacking::Worktree { git, .. } | ManagedBacking::Clone { git },
                } => git,
                _ => return Err(Error::Conflict),
            };
            if git(&path, &["symbolic-ref", "--short", "HEAD"], "inspect")?.trim()
                != intent.effective_branch(&id)
            {
                return Err(Error::Conflict);
            }
            if let Ok(marker) = git(&path, &["config", "--get", "hive.workspace"], "inspect")
                && marker.trim() != id.as_str()
            {
                return Err(Error::Conflict);
            }
            // Claim only the exact registered repository/branch/commit dispatched by this operation.
            git(
                &path,
                &[
                    "config",
                    if backend == "clone" {
                        "--local"
                    } else {
                        "--worktree"
                    },
                    "hive.workspace",
                    id.as_str(),
                ],
                "ownership",
            )?;
            let location = self.location(&entry, &path, Some(commit))?;
            self.state
                .operations
                .get_mut(&key)
                .ok_or(Error::Conflict)?
                .path = Some(path);
            entry
                .workspace
                .transition(WorkspaceEvent::ReconciledReady { location })?;
            entry.failure = None;
        } else {
            return Err(Error::Request);
        }
        self.state.workspaces.insert(id.to_string(), entry.clone());
        self.save()?;
        Ok(entry)
    }

    fn finish_cleanup(&self, entry: &mut Entry, operation: &OperationId) -> Result<(), Error> {
        if entry.workspace.state() == WorkspaceState::CleanupFailed {
            entry
                .workspace
                .transition(WorkspaceEvent::ReconciledRetry)?;
            entry.workspace.transition(WorkspaceEvent::Cleanup {
                operation: operation.clone(),
            })?;
        }
        entry.workspace.transition(WorkspaceEvent::Cleaned)?;
        entry.failure = None;
        Ok(())
    }
}

fn refresh_remote_head(path: &Path, operation: &'static str) -> Result<(), Error> {
    // Fetch alone does not update origin/HEAD when the remote changes its default branch.
    let head = git(
        path,
        &["ls-remote", "--symref", "origin", "HEAD"],
        operation,
    )?;
    let branch = head
        .lines()
        .find_map(|line| {
            line.strip_prefix("ref: refs/heads/")
                .and_then(|rest| rest.split_once('\t'))
                .map(|(branch, _)| branch)
        })
        .ok_or(Error::Git(operation))?;
    let target = format!("refs/remotes/origin/{branch}");
    git(
        path,
        &[
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{target}^{{commit}}"),
        ],
        operation,
    )?;
    git(
        path,
        &["symbolic-ref", "refs/remotes/origin/HEAD", &target],
        operation,
    )?;
    Ok(())
}

fn resolve(path: &Path, reference: &GitRef) -> Result<String, Error> {
    // Prefer fetched upstream branches over stale local task/default branches.
    for candidate in [
        format!("refs/remotes/origin/{}", reference.as_str()),
        reference.as_str().to_owned(),
    ] {
        if let Ok(commit) = git(
            path,
            &[
                "rev-parse",
                "--verify",
                "--end-of-options",
                &format!("{candidate}^{{commit}}"),
            ],
            "ref resolution",
        ) {
            return Ok(commit.trim().into());
        }
    }
    Err(Error::Request)
}

fn probe_worktrunk() -> WorktrunkAvailability {
    let Ok(help) = execute("wt", None, &["switch", "--help"], true) else {
        return WorktrunkAvailability::Unavailable;
    };
    let removal = execute("wt", None, &["remove", "--help"], true).unwrap_or_default();
    let approvals =
        execute("wt", None, &["config", "approvals", "list", "--help"], true).unwrap_or_default();
    if help.contains("--format")
        && help.contains("--no-cd")
        && help.contains("--create")
        && removal.contains("--foreground")
        && removal.contains("--no-delete-branch")
        && removal.contains("--format")
        && approvals.contains("--format")
    {
        WorktrunkAvailability::Usable
    } else {
        WorktrunkAvailability::Incompatible
    }
}

fn check_approvals(path: &Path) -> Result<(), Error> {
    check_approvals_with_config(path, None)
}

fn check_approvals_with_config(path: &Path, config: Option<&Path>) -> Result<(), Error> {
    let output = wt_with_config(
        path,
        &["config", "approvals", "list", "--format=json"],
        config,
    )?;
    let value: serde_json::Value = serde_json::from_str(&output).map_err(|_| Error::Worktrunk)?;
    if matches!(value["state"].as_str(), Some("approved" | "no_commands")) {
        Ok(())
    } else {
        Err(Error::Worktrunk)
    }
}

fn wt_with_config(path: &Path, args: &[&str], config: Option<&Path>) -> Result<String, Error> {
    execute_with_config("wt", Some(path), args, true, config)
}
fn git(path: &Path, args: &[&str], operation: &'static str) -> Result<String, Error> {
    execute("git", Some(path), args, false).map_err(|error| match error {
        Error::Git(_) => Error::Git(operation),
        other => other,
    })
}

fn execute(
    program: &str,
    path: Option<&Path>,
    args: &[&str],
    worktrunk: bool,
) -> Result<String, Error> {
    execute_with_config(program, path, args, worktrunk, None)
}

fn execute_with_config(
    program: &str,
    path: Option<&Path>,
    args: &[&str],
    worktrunk: bool,
    project_config: Option<&Path>,
) -> Result<String, Error> {
    #[cfg(test)]
    let replacement = TEST_GIT_CONFIG.with(|config| {
        config
            .borrow()
            .as_ref()
            .and_then(|path| path.parent())
            .map(|parent| parent.join("fake-wt"))
            .filter(|path| program == "wt" && path.is_file())
    });
    let mut command = Command::new({
        #[cfg(test)]
        {
            replacement.as_deref().unwrap_or_else(|| Path::new(program))
        }
        #[cfg(not(test))]
        {
            program
        }
    });
    if let Some(config) = project_config {
        command.env("WORKTRUNK_PROJECT_CONFIG_PATH", config);
    }
    #[cfg(test)]
    TEST_GIT_CONFIG.with(|config| {
        if let Some(path) = config.borrow().as_ref() {
            command
                .env("GIT_CONFIG_GLOBAL", path)
                .env("GIT_CONFIG_NOSYSTEM", "1");
            if let Some(parent) = path.parent() {
                command
                    .env("HOME", parent)
                    .env("XDG_CONFIG_HOME", parent.join("config"));
            }
        }
    });
    // Git for Windows requires opt-in long-path handling even for fresh repositories.
    // Pass portable argv spellings while retaining canonical filesystem identities in state.
    #[cfg(windows)]
    {
        if program == "git" {
            command.args(["-c", "core.longpaths=true"]);
        }
        command.args(args.iter().map(|arg| git_path_argument(arg)));
    }
    #[cfg(not(windows))]
    command.args(args);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never");
    if let Some(path) = path {
        command.current_dir(path);
    }
    let (status, output, diagnostic) =
        capture_command(command, Duration::from_secs(120), worktrunk)?;
    if !status.success() {
        // Classify without exposing stderr: remote helpers and hooks can print secrets.
        let diagnostic = String::from_utf8_lossy(&diagnostic).to_ascii_lowercase();
        return Err(if worktrunk {
            Error::Worktrunk
        } else if diagnostic.contains("authentication failed")
            || diagnostic.contains("permission denied (publickey)")
        {
            Error::Authentication
        } else {
            Error::Git("execution")
        });
    }
    String::from_utf8(output).map_err(|_| Error::Uncertain)
}

fn registered_worktree(list: &str, expected: &Path) -> Result<bool, Error> {
    // Missing leaves still need a physical parent identity to detect stale registrations
    // after an interrupted removal; canonicalizing only existing worktrees is insufficient.
    fn identity(path: &Path) -> std::io::Result<PathBuf> {
        let mut current = path;
        let mut suffix = Vec::new();
        loop {
            match current.canonicalize() {
                Ok(mut root) => {
                    root.extend(suffix.into_iter().rev());
                    return Ok(root);
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    suffix.push(current.file_name().ok_or(error)?.to_owned());
                    current = current
                        .parent()
                        .ok_or_else(|| std::io::Error::other("no path parent"))?;
                }
                Err(error) => return Err(error),
            }
        }
    }
    let expected = identity(expected).map_err(|_| Error::Conflict)?;
    for path in list
        .split('\0')
        .filter_map(|field| field.strip_prefix("worktree "))
    {
        if identity(Path::new(path)).map_err(|_| Error::Conflict)? == expected {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(any(windows, test))]
fn git_path_argument(argument: &str) -> String {
    if let Some(path) = argument.strip_prefix(r"\\?\UNC\") {
        return format!("//{}", path.replace('\\', "/"));
    }
    if let Some(path) = argument.strip_prefix(r"\\?\") {
        return path.replace('\\', "/");
    }
    if argument.as_bytes().get(1) == Some(&b':')
        && matches!(argument.as_bytes().get(2), Some(b'\\' | b'/'))
    {
        return argument.replace('\\', "/");
    }
    argument.into()
}

fn capture_command(
    command: Command,
    limit: Duration,
    worktrunk: bool,
) -> Result<(std::process::ExitStatus, Vec<u8>, Vec<u8>), Error> {
    use command_group::AsyncCommandGroup;
    use tokio::io::AsyncReadExt;

    // A dedicated, joined runtime works both inside the service's blocking job and
    // for synchronous callers already running on Tokio, without detached reader threads.
    std::thread::scope(|scope| {
        scope
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|_| Error::Storage)?;
                runtime.block_on(async move {
                    let mut command = tokio::process::Command::from(command);
                    command.kill_on_drop(true);
                    let mut child = command.group_spawn().map_err(|_| {
                        if worktrunk {
                            Error::Worktrunk
                        } else {
                            Error::Git("execution")
                        }
                    })?;
                    async fn drain(
                        mut pipe: impl tokio::io::AsyncRead + Unpin,
                    ) -> std::io::Result<Vec<u8>> {
                        let mut output = Vec::new();
                        let mut buffer = [0; 8192];
                        loop {
                            let count = pipe.read(&mut buffer).await?;
                            if count == 0 {
                                return Ok(output);
                            }
                            let retained =
                                count.min((1024 * 1024_usize).saturating_sub(output.len()));
                            output.extend_from_slice(&buffer[..retained]);
                        }
                    }
                    let stdout = child.inner().stdout.take().ok_or(Error::Uncertain)?;
                    let stderr = child.inner().stderr.take().ok_or(Error::Uncertain)?;
                    // Wait for the leader and both EOFs, not all background hooks. WorkTrunk's
                    // successfully detached hooks keep their documented asynchronous lifetime.
                    let result = tokio::time::timeout(limit, async {
                        tokio::try_join!(child.inner().wait(), drain(stdout), drain(stderr))
                    })
                    .await;
                    match result {
                        Ok(Ok(output)) => Ok(output),
                        _ => {
                            // Kill the Unix process group / Windows Job even if its leader already
                            // exited. Cancelling the read futures closes pipes instead of leaking readers.
                            let _ = child.start_kill();
                            let _ =
                                tokio::time::timeout(Duration::from_secs(5), child.inner().wait())
                                    .await;
                            Err(Error::Uncertain)
                        }
                    }
                })
            })
            .join()
            .map_err(|_| Error::Uncertain)?
    })
}

#[cfg(test)]
mod tests;
