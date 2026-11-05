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
use hive_core::workspace::backend::{
    BackendData, BackendSelection, CheckpointWriter, ProvisionerId, RepositoryHandle,
};
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
mod backends;
mod persistence;
mod recovery;
mod routes;
pub use recovery::{RecoveryReport, Sweep, SweepRequest};
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
                revision: None,
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
                revision,
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
                    revision: revision
                        .map(hive_core::project::Revision::try_from)
                        .transpose()
                        .map_err(|_| Error::Request)?,
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
        "JJ is unavailable or lacks required capabilities; install Jujutsu 0.46 or newer on the execution node"
    )]
    JjUnavailable,
    #[error(
        "JJ command failed; check repository compatibility, node-local credentials and permissions, then inspect before retrying"
    )]
    Jj,
    #[error(
        "JJ base revision did not resolve to one commit; select a single existing revision or change ID"
    )]
    JjRevision,
    #[error(
        "invalid _meta.hive.workspace selector; use kind 'new' with optional project/policy/baseRef/branch/revision, 'attach' with id, or 'existing' with directory"
    )]
    Selector,
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
        "resource identity conflicts with the recorded repository, path or SCM identity; inspect the resource before reconciling"
    )]
    Conflict,
    #[error(
        "operation key has different durable intent; replay the original request or use a new operation key"
    )]
    RetryConflict,
    #[error(
        "invalid workspace policy, path or revision; check project SCM/policy compatibility and the requested base"
    )]
    Request,
    #[error(
        "WorkTrunk command failed; check wt compatibility, hook approvals and project hooks, then inspect before retrying"
    )]
    Worktrunk,
    #[error(
        "workspace cleanup filesystem operation failed; check checkout/archive permissions, disk space and open files, inspect the cleanup operation, then retry with the same operation key"
    )]
    Removal,
    #[error(transparent)]
    Domain(#[from] hive_core::workspace::WorkspaceError),
}

impl Error {
    pub fn code(&self) -> &'static str {
        match self {
            Self::JjUnavailable => "hive::jj::unavailable",
            Self::Jj => "hive::jj::execution",
            Self::JjRevision => "hive::jj::revision",
            Self::Selector => "hive::workspace::selector",
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
            Self::Removal => "hive::workspace::removal",
            Self::Domain(error) => error.code(),
        }
    }

    pub fn json(&self) -> serde_json::Value {
        let help = match self {
            Self::JjUnavailable => {
                "install JJ 0.46 or newer on the owning node and ensure its executable is on the daemon's PATH"
            }
            Self::JjRevision => {
                "use a single conflict-free JJ revision/change ID; inspect bookmarks and narrow ambiguous revsets"
            }
            Self::Jj => {
                "check the owning node's JJ/Git/SSH configuration and repository permissions, then inspect the original operation before retrying"
            }
            Self::Domain(error) => error.help(),
            Self::Selector => {
                "send a supported _meta.hive.workspace object with correctly typed fields; see the session launch selectors in Hive's API documentation"
            }
            _ => {
                "inspect the resource and original operation before retrying; configure credentials and hook approvals on the execution node"
            }
        };
        serde_json::json!({"code":self.code(),"message":self.to_string(),"help":help})
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Repository {
    #[serde(flatten)]
    pub repository: NodeRepository,
    pub source: String,
    pub status: String,
    pub failure: Option<String>,
    #[serde(default)]
    pub handle: Option<RepositoryHandle>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Entry {
    pub workspace: Workspace,
    pub repository: Option<RepositoryId>,
    pub failure: Option<String>,
    #[serde(default)]
    pub archive: Option<NodePath>,
    #[serde(default)]
    pub selection: Option<BackendSelection>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<hive_core::project::Revision>,
    pub operation: OperationId,
    pub project: ProjectId,
    pub policy: WorkspaceTemplate,
    pub base_ref: Option<GitRef>,
    pub branch: Option<GitRef>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum OperationKind {
    Create,
    Cleanup,
}

#[derive(Clone, Serialize, Deserialize)]
struct Operation {
    request: serde_json::Value,
    workspace: WorkspaceId,
    // Pin resolution before creating branches, so a retry cannot silently change its base.
    commit: Option<String>,
    path: Option<PathBuf>,
    effects_started: bool,
    #[serde(default)]
    selection: Option<BackendSelection>,
    #[serde(default)]
    kind: Option<OperationKind>,
    #[serde(default)]
    prepared: Option<BackendData>,
}

impl Operation {
    fn is_create(&self) -> bool {
        matches!(self.kind, Some(OperationKind::Create))
    }
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
    #[serde(default)]
    version: u32,
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
    #[serde(default)]
    lifecycle: BTreeMap<String, Vec<recovery::Transition>>,
    #[serde(default)]
    last_used: BTreeMap<String, u64>,
    #[serde(default)]
    sweeps: BTreeMap<String, Sweep>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            version: 1,
            node: NodeId::new(),
            repositories: BTreeMap::new(),
            workspaces: BTreeMap::new(),
            operations: BTreeMap::new(),
            uses: BTreeMap::new(),
            protected: BTreeMap::new(),
            archives: BTreeMap::new(),
            dispatched: BTreeSet::new(),
            lifecycle: BTreeMap::new(),
            last_used: BTreeMap::new(),
            sweeps: BTreeMap::new(),
        }
    }
}

/// The blocking job owns the filesystem guard even if the HTTP/ACP caller disconnects.
#[derive(Clone)]
pub struct Service {
    store: Arc<Store>,
    manager: Arc<hive_core::workspace::backend::BackendManager<Error>>,
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
            manager: Arc::new(backends::manager()),
            #[cfg(test)]
            git_config,
        }
    }

    async fn run<T: Send + 'static>(
        &self,
        work: impl FnOnce(&mut Runtime<'_>) -> Result<T, Error> + Send + 'static,
    ) -> Result<T, Error> {
        let store = self.store.clone();
        let manager = self.manager.clone();
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
                manager: &manager,
                state,
            };
            let migrated = runtime.migrate_backends()?;
            if initialize || migrated {
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
            Ok(serde_json::json!({"operation":id,"request":operation.request,"workspace":operation.workspace,"commit":operation.commit,"path":operation.path,"selection":operation.selection,"prepared":operation.prepared,"effectsStarted":operation.effects_started,"sessions":uses,"worktrunkConfig":config.is_file().then_some(config)}))
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
            let affected: Vec<_> = rt
                .state
                .workspaces
                .values()
                .filter(|entry| rt.holds(&session, entry.workspace.id()))
                .map(|entry| entry.workspace.id().clone())
                .collect();
            rt.state.uses.remove(&session);
            rt.state.protected.remove(&session);
            rt.state.dispatched.remove(&session);
            for id in affected {
                if !rt.has_use(&id) {
                    rt.touch(&id);
                }
            }
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
    manager: &'a hive_core::workspace::backend::BackendManager<Error>,
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
                        scm: hive_core::project::Scm::Git,
                        default_revision: None,
                        name: "Registered workspace".into(),
                        repository_source: source,
                        default_ref: None,
                        preparation: vec![],
                    },
                    // Retry dispatch uses the persisted selection before configuration is consulted.
                    WorktrunkConfig::default(),
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
                                    && operation.is_create()
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
        for id in &protected {
            self.touch(id);
        }
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
    fn save(&mut self) -> Result<(), Error> {
        self.record_transitions();
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
        let manager = self.manager;
        let handle = self.repository_handle(repo)?;
        manager.repository(handle)?.verify(handle)
    }
    fn fetch(&mut self, repo: &mut Repository) -> Result<(), Error> {
        self.verify_repository(repo)?;
        repo.status = "fetching".into();
        repo.failure = None;
        self.state
            .repositories
            .insert(repo.repository.id.to_string(), repo.clone());
        self.save()?;
        let manager = self.manager;
        let handle = self.repository_handle(repo)?;
        let result = manager.repository(handle)?.update(handle);
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
            .find(|r| {
                &r.repository.project == project
                    && &r.source == source
                    && r.handle.as_ref().is_some_and(|h| {
                        h.backend.0
                            == match definition.scm {
                                hive_core::project::Scm::Git => "git",
                                hive_core::project::Scm::Jj => "jj",
                            }
                    })
            })
            .cloned()
        {
            return Ok(repo);
        }
        let id = RepositoryId::new();
        // Project slugs are unbounded portable identifiers, not filesystem components.
        // Stable resource IDs avoid inflating Windows linked-worktree metadata paths.
        let project_root = self.root().join(id.as_str());
        let mut repo = Repository {
            repository: NodeRepository {
                id,
                project: project.clone(),
                root: self.path(&project_root.join(match definition.scm {
                    hive_core::project::Scm::Git => ".git",
                    hive_core::project::Scm::Jj => "jj",
                }))?,
                role: RepositoryRole::Cache,
                ownership: Ownership::HiveManaged,
            },
            source: source.clone(),
            status: "cloning".into(),
            failure: None,
            handle: None,
        };
        repo.handle = Some(match definition.scm {
            hive_core::project::Scm::Git => backends::handle(&repo)?,
            hive_core::project::Scm::Jj => backends::jj::handle(&repo)?,
        });
        self.state
            .repositories
            .insert(repo.repository.id.to_string(), repo.clone());
        // Registration is inert; publish it atomically with the initial workspace and operation.
        Ok(repo)
    }
    fn acquire(&mut self, mut repo: Repository) -> Result<Repository, Error> {
        let manager = self.manager;
        let handle = self.repository_handle(&repo)?;
        let result = manager.repository(handle)?.acquire(handle);
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
        if !self.state.operations.contains_key(&key) && self.operation_reserved(&request.operation)
        {
            return Err(Error::RetryConflict);
        }
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
                match self.provision(&entry, &key) {
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
                Inspection::Present { .. }
                    if original.effects_started && !self.uncertain_backend(&entry)? =>
                {
                    let mut entry = entry;
                    entry.failure = None;
                    let location = self.completed_creation(&entry)?.ok_or(Error::Uncertain)?;
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
        let jj = matches!(&request.policy, WorkspaceTemplate::Jj { .. })
            || matches!(&request.policy, WorkspaceTemplate::Ephemeral { backing, .. } if backing == "jj");
        if !request.policy.supports_scm(definition.scm)
            || (jj && (request.base_ref.is_some() || request.branch.is_some()))
            || (!jj && request.revision.is_some())
            || matches!(&request.policy, WorkspaceTemplate::Ephemeral { backing, base_ref: Some(_), .. } if backing == "jj")
        {
            return Err(Error::Request);
        }
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
        let retention = match &request.policy {
            WorkspaceTemplate::Ephemeral { retention, .. } => retention.clone(),
            _ => Retention::Retain,
        };
        let existing = matches!(request.policy, WorkspaceTemplate::Existing { .. });
        let clone = matches!(&request.policy, WorkspaceTemplate::Clone { .. })
            || matches!(&request.policy, WorkspaceTemplate::Ephemeral { backing, .. } if backing == "clone");
        let backend = if existing {
            "existing"
        } else if jj {
            "jj.workspace"
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
                selection: None,
                kind: Some(OperationKind::Create),
                prepared: None,
            },
        );
        let policy = if let Some(directory) = existing_directory {
            WorkspacePolicy::Existing { directory }
        } else {
            let repo = self.repository(&request.project, &definition)?;
            let selection = BackendSelection {
                provisioner: if jj {
                    hive_core::workspace::backend::ProvisionerId("jj.workspace".into())
                } else {
                    persistence::legacy_provisioner(&BackendId(backend.into()))?
                },
                repository: repo.handle.clone(),
            };
            self.manager.resolve(&selection)?;
            let intent = if jj {
                let configured = match &request.policy {
                    WorkspaceTemplate::Jj { revision } => revision.clone(),
                    _ => None,
                };
                let revision = request
                    .revision
                    .clone()
                    .or(configured)
                    .or(definition.default_revision.clone())
                    .unwrap_or(
                        hive_core::project::Revision::try_from("trunk()".to_owned())
                            .map_err(|_| Error::Request)?,
                    );
                backends::data("hive.jj.intent", &revision)?
            } else {
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
                backends::data("hive.git.intent", &git)?
            };
            repository = Some(repo.repository.id.clone());
            if matches!(request.policy, WorkspaceTemplate::Ephemeral { .. }) {
                WorkspacePolicy::Ephemeral {
                    backing: ManagedBacking::Backend {
                        selection: Box::new(selection),
                        intent,
                    },
                }
            } else {
                WorkspacePolicy::Managed {
                    selection: Box::new(selection),
                    intent,
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
            repository: repository.clone(),
            failure: None,
            archive: None,
            selection: Some(BackendSelection {
                provisioner: if jj {
                    hive_core::workspace::backend::ProvisionerId("jj.workspace".into())
                } else {
                    persistence::legacy_provisioner(&BackendId(backend.into()))?
                },
                repository: repository
                    .as_ref()
                    .map(|id| {
                        self.state
                            .repositories
                            .get(id.as_str())
                            .and_then(|r| r.handle.clone())
                            .ok_or(Error::Conflict)
                    })
                    .transpose()?,
            }),
        };
        self.manager
            .resolve(entry.selection.as_ref().ok_or(Error::Conflict)?)?;
        self.state
            .operations
            .get_mut(&key)
            .ok_or(Error::Conflict)?
            .selection = entry.selection.clone();
        self.state.workspaces.insert(id.to_string(), entry.clone());
        self.save()?;
        let result = self.provision(&entry, &key);
        match result {
            Ok(location) => {
                #[cfg(test)]
                recovery::crash_boundary("provisioned");
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
                self.state.workspaces.insert(id.to_string(), entry);
                self.save()?;
                return Err(error);
            }
        }
        self.state.workspaces.insert(id.to_string(), entry.clone());
        self.save()?;
        Ok(entry)
    }

    fn provision(&mut self, entry: &Entry, key: &str) -> Result<WorkspaceLocation, Error> {
        let selection = self.selection(entry)?;
        let backend = entry.workspace.backend().ok_or(Error::Conflict)?.clone();
        let manager = self.manager;
        let intent = backends::intent(entry)?;
        manager.provisioner(&selection)?.validate_intent(&intent)?;
        if let Some(repository) = &selection.repository {
            let mut repo = self
                .state
                .repositories
                .get(repository.id.as_str())
                .ok_or(Error::Conflict)?
                .clone();
            if Path::new(repo.repository.root.as_str()).exists() {
                self.fetch(&mut repo)?;
            } else {
                self.acquire(repo)?;
            }
        }
        let directory = self.path(self.store.state_directory())?;
        let operation_id = OperationId::try_from(key.to_owned()).map_err(|_| Error::Request)?;
        let previous = self
            .state
            .operations
            .get(key)
            .ok_or(Error::Conflict)?
            .prepared
            .clone();
        let provisioner = manager.provisioner(&selection)?;
        let prepared = provisioner.prepare(
            selection.repository.as_ref(),
            &intent,
            hive_core::workspace::backend::PreparationContext {
                workspace: entry.workspace.id(),
                operation: &operation_id,
                directory: &directory,
                previous: previous.as_ref(),
            },
            &mut persistence::OperationCheckpoints { runtime: self, key },
        )?;
        let operation = self.state.operations.get_mut(key).ok_or(Error::Conflict)?;
        operation.prepared = Some(prepared.intent.clone());
        operation.path = prepared
            .path
            .as_ref()
            .map(|path| PathBuf::from(path.as_str()));
        operation.effects_started = !manager
            .resolve(&selection)?
            .capabilities
            .uncertain_external_effects;
        self.save()?;
        let observation = provisioner.create(
            selection.repository.as_ref(),
            &prepared.intent,
            &mut persistence::OperationCheckpoints { runtime: self, key },
        )?;
        entry
            .workspace
            .intent()
            .check_node(observation.root.node())?;
        entry
            .workspace
            .intent()
            .check_node(observation.working_directory.node())?;
        let pinned = self
            .state
            .operations
            .get(key)
            .and_then(|o| o.commit.as_deref());
        backends::location(observation, backend, pinned)
    }

    #[cfg(test)]
    fn location(
        &self,
        entry: &Entry,
        path: &Path,
        expected: Option<&str>,
    ) -> Result<WorkspaceLocation, Error> {
        let observation = self.observe_at(entry, path, expected.is_some())?;
        let pinned = self.creation(entry)?.commit.as_deref();
        backends::location(
            observation,
            entry.workspace.backend().cloned().ok_or(Error::Conflict)?,
            pinned,
        )
    }

    fn creation(&self, entry: &Entry) -> Result<&Operation, Error> {
        self.state
            .operations
            .values()
            .find(|operation| &operation.workspace == entry.workspace.id() && operation.is_create())
            .ok_or(Error::Conflict)
    }

    fn observe_at(
        &self,
        entry: &Entry,
        path: &Path,
        verify_creation: bool,
    ) -> Result<hive_core::workspace::backend::WorkspaceObservation, Error> {
        let selection = self.selection(entry)?;
        let manager = self.manager;
        let root = self.path(path)?;
        let prepared = self
            .creation(entry)?
            .prepared
            .as_ref()
            .ok_or(Error::Conflict)?;
        let provisioner = manager.provisioner(&selection)?;
        let intent = provisioner.at_location(prepared, &root, verify_creation)?;
        let observation = provisioner.inspect(selection.repository.as_ref(), &intent)?;
        // Backend evidence must remain node-local before it can participate in cleanup admission.
        entry
            .workspace
            .intent()
            .check_node(observation.root.node())?;
        entry
            .workspace
            .intent()
            .check_node(observation.working_directory.node())?;
        Ok(observation)
    }

    fn inspect(&self, id: &WorkspaceId) -> Result<Inspection, Error> {
        let entry = self
            .state
            .workspaces
            .get(id.as_str())
            .ok_or(Error::Unknown)?;
        self.selection(entry)?;
        if entry.workspace.intent().node != self.state.node {
            return Ok(Inspection::IdentityConflict);
        }
        let operation = self
            .state
            .operations
            .values()
            .find(|o| &o.workspace == id && o.is_create());
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
        // A dangling symlink is a conflicting directory entry, not proven absence.
        let metadata = match path.symlink_metadata() {
            Ok(metadata) => Some(metadata),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => return Err(Error::Conflict),
        };
        if metadata.is_none() {
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
        if metadata.is_some_and(|metadata| metadata.file_type().is_symlink()) {
            return Ok(Inspection::IdentityConflict);
        }
        let observation = match self.observe_at(entry, &path, false) {
            Ok(observation) => observation,
            Err(error @ Error::Domain(_)) => return Err(error),
            Err(_) => return Ok(Inspection::IdentityConflict),
        };
        let cleanliness = observation.cleanliness;
        let ownership_verified = observation.ownership_verified;
        let location = backends::location(
            observation,
            entry.workspace.backend().cloned().ok_or(Error::Conflict)?,
            self.creation(entry)?.commit.as_deref(),
        )?;
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
                ownership_verified,
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
        let selection = self.selection(&entry)?;
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
        if !self.state.operations.contains_key(operation.as_str())
            && self.operation_reserved(&operation)
            && !self.planned_cleanup(&cleanup)
        {
            return Err(Error::RetryConflict);
        }
        if let Some(original) = self.state.operations.get(operation.as_str()) {
            if original.request != request {
                return Err(Error::RetryConflict);
            }
            if entry.workspace.state() == WorkspaceState::Removed {
                return Ok(entry);
            }
            if self.uncertain_backend(&entry)? && original.effects_started {
                return Err(Error::Uncertain);
            }
            if !matches!(
                entry.workspace.state(),
                WorkspaceState::Cleaning | WorkspaceState::CleanupFailed
            ) {
                return Err(Error::Conflict);
            }
            if !self.has_use(&id)
                && entry.workspace.location().is_some_and(|location| {
                    std::fs::symlink_metadata(location.root.as_str())
                        .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
                })
            {
                // Only an explicit retry can finish native deregistration. Startup remains read-only.
                let intent = self
                    .creation(&entry)?
                    .prepared
                    .as_ref()
                    .ok_or(Error::Conflict)?;
                self.manager.provisioner(&selection)?.finish_removal(
                    selection.repository.as_ref(),
                    intent,
                    original.prepared.as_ref(),
                    &operation,
                )?;
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
        let manager = self.manager;
        let provisioner = manager.provisioner(&selection)?;
        let removal_intent = provisioner.at_location(
            self.creation(&entry)?
                .prepared
                .as_ref()
                .ok_or(Error::Conflict)?,
            &location.root,
            false,
        )?;
        provisioner.prepare_cleanup(selection.repository.as_ref(), &removal_intent)?;
        entry.workspace.transition(WorkspaceEvent::Cleanup {
            operation: operation.clone(),
        })?;
        let prior_checkpoint = self.cleanup_checkpoint(&operation)?;
        self.state.operations.insert(
            operation.to_string(),
            Operation {
                request,
                workspace: id.clone(),
                commit: None,
                path: Some(PathBuf::from(location.root.as_str())),
                effects_started: true,
                selection: entry.selection.clone(),
                kind: Some(OperationKind::Cleanup),
                prepared: prior_checkpoint,
            },
        );
        self.state.workspaces.insert(id.to_string(), entry.clone());
        self.save()?;
        let path = Path::new(location.root.as_str());
        let result = (|| {
            if !discard {
                let directory = self.path(self.store.state_directory())?;
                let previous = self.cleanup_checkpoint(&operation)?;
                let archive = provisioner.preserve(
                    selection.repository.as_ref(),
                    &removal_intent,
                    hive_core::workspace::backend::PreparationContext {
                        workspace: &id,
                        operation: &operation,
                        directory: &directory,
                        previous: previous.as_ref(),
                    },
                    &mut persistence::OperationCheckpoints {
                        runtime: self,
                        key: operation.as_str(),
                    },
                )?;
                if let Some(archive) = archive {
                    entry.archive = Some(archive);
                    self.state.workspaces.insert(id.to_string(), entry.clone());
                    self.save()?;
                }
            }
            let manager = self.manager;
            let provisioner = manager.provisioner(&selection)?;
            let intent = provisioner.at_location(
                self.creation(&entry)?
                    .prepared
                    .as_ref()
                    .ok_or(Error::Conflict)?,
                &location.root,
                false,
            )?;
            provisioner.remove_checkpointed(
                selection.repository.as_ref(),
                &intent,
                discard,
                &operation,
                &mut persistence::OperationCheckpoints {
                    runtime: self,
                    key: operation.as_str(),
                },
            )
        })();
        let result = result.and_then(|()| {
            if path.try_exists().map_err(|_| Error::Conflict)? {
                Err(Error::Uncertain)
            } else {
                Ok(())
            }
        });
        #[cfg(test)]
        if result.is_ok() {
            recovery::crash_boundary("deleted");
        }
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

    fn verified_removal_absence(&self, entry: &Entry) -> Result<bool, Error> {
        let selection = self.selection(entry)?;
        let location = entry.workspace.location().ok_or(Error::Conflict)?;
        match Path::new(location.root.as_str()).symlink_metadata() {
            Ok(_) => return Ok(false),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(Error::Conflict),
        }
        if self.has_use(entry.workspace.id()) {
            return Err(Error::Conflict);
        }
        let operation = self
            .state
            .operations
            .get(entry.workspace.operation().ok_or(Error::Conflict)?.as_str())
            .ok_or(Error::Conflict)?;
        let request: CleanupRequest =
            serde_json::from_value(operation.request.clone()).map_err(|_| Error::Conflict)?;
        if request.mode != CleanupMode::DiscardChanges
            && !self.verified_archive(entry, &request.operation)
        {
            return Ok(false);
        }
        let manager = self.manager;
        let provisioner = manager.provisioner(&selection)?;
        let intent = provisioner.at_location(
            self.creation(entry)?
                .prepared
                .as_ref()
                .ok_or(Error::Conflict)?,
            &location.root,
            false,
        )?;
        provisioner.is_absent(selection.repository.as_ref(), &intent)
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
        let selection = self.selection(&entry)?;
        if self.uncertain_backend(&entry)? && !hooks_reconciled {
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
            let manager = self.manager;
            let provisioner = manager.provisioner(&selection)?;
            let intent = provisioner.at_location(
                operation.prepared.as_ref().ok_or(Error::Conflict)?,
                &self.path(&path)?,
                true,
            )?;
            let observation = provisioner.reconcile(selection.repository.as_ref(), &intent)?;
            let location = backends::location(
                observation,
                entry.workspace.backend().cloned().ok_or(Error::Conflict)?,
                operation.commit.as_deref(),
            )?;
            self.state
                .operations
                .get_mut(&key)
                .ok_or(Error::Conflict)?
                .path = Some(path);
            // Publish the verified binding so subsequent inspection never needs a guessed path.
            self.state
                .operations
                .get_mut(&key)
                .ok_or(Error::Conflict)?
                .prepared = Some(intent);
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
    String::from_utf8(execute_output(
        program,
        path,
        args,
        worktrunk,
        project_config,
    )?)
    .map_err(|_| Error::Uncertain)
}

fn execute_output(
    program: &str,
    path: Option<&Path>,
    args: &[&str],
    worktrunk: bool,
    project_config: Option<&Path>,
) -> Result<Vec<u8>, Error> {
    execute_output_mode(program, path, args, worktrunk, project_config, false)
}

fn execute_output_mode(
    program: &str,
    path: Option<&Path>,
    args: &[&str],
    worktrunk: bool,
    project_config: Option<&Path>,
    digest: bool,
) -> Result<Vec<u8>, Error> {
    #[cfg(test)]
    let replacement = TEST_GIT_CONFIG.with(|config| {
        config
            .borrow()
            .as_ref()
            .and_then(|path| path.parent())
            .map(|parent| parent.join(format!("fake-{program}")))
            .filter(|path| matches!(program, "wt" | "jj") && path.is_file())
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
                    .env("XDG_CONFIG_HOME", parent.join("config"))
                    .env("JJ_CONFIG", parent.join("jj.toml"));
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
        capture_output(command, Duration::from_secs(120), worktrunk, digest).map_err(|error| {
            if program == "jj" && matches!(error, Error::Git(_)) {
                Error::JjUnavailable
            } else {
                error
            }
        })?;
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
            if program == "jj" {
                Error::Jj
            } else {
                Error::Git("execution")
            }
        });
    }
    // JJ may report unreadable file contents as a warning with a successful exit.
    // Its empty stdout must never be accepted as the digest of an empty tracked file.
    if digest && !diagnostic.is_empty() {
        return Err(Error::Uncertain);
    }
    Ok(output)
}

fn physical_path(path: &Path) -> std::io::Result<PathBuf> {
    // Missing leaves still need a physical parent identity to detect stale registrations
    // after an interrupted removal; canonicalizing only existing worktrees is insufficient.
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

fn registered_worktree(list: &str, expected: &Path) -> Result<bool, Error> {
    let expected = physical_path(expected).map_err(|_| Error::Conflict)?;
    for path in list
        .split('\0')
        .filter_map(|field| field.strip_prefix("worktree "))
    {
        if physical_path(Path::new(path)).map_err(|_| Error::Conflict)? == expected {
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

#[cfg(test)]
fn capture_command(
    command: Command,
    limit: Duration,
    worktrunk: bool,
) -> Result<(std::process::ExitStatus, Vec<u8>, Vec<u8>), Error> {
    capture_output(command, limit, worktrunk, false)
}

fn capture_output(
    command: Command,
    limit: Duration,
    worktrunk: bool,
    digest: bool,
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
                        digest: bool,
                    ) -> std::io::Result<Vec<u8>> {
                        use sha2::{Digest, Sha256};
                        let mut hash = Sha256::new();
                        let mut output = Vec::new();
                        let mut truncated = false;
                        let mut buffer = [0; 8192];
                        loop {
                            let count = pipe.read(&mut buffer).await?;
                            if count == 0 {
                                if digest {
                                    return Ok(hash.finalize().to_vec());
                                }
                                // A truncated manifest cannot be evidence that a checkout is clean.
                                return if truncated {
                                    Err(std::io::Error::other("command output limit exceeded"))
                                } else {
                                    Ok(output)
                                };
                            }
                            if digest {
                                hash.update(&buffer[..count]);
                                continue;
                            }
                            let retained =
                                count.min((1024 * 1024_usize).saturating_sub(output.len()));
                            output.extend_from_slice(&buffer[..retained]);
                            truncated |= retained != count;
                        }
                    }
                    let stdout = child.inner().stdout.take().ok_or(Error::Uncertain)?;
                    let stderr = child.inner().stderr.take().ok_or(Error::Uncertain)?;
                    // Wait for the leader and both EOFs, not all background hooks. WorkTrunk's
                    // successfully detached hooks keep their documented asynchronous lifetime.
                    let result = tokio::time::timeout(limit, async {
                        tokio::try_join!(
                            child.inner().wait(),
                            drain(stdout, digest),
                            drain(stderr, false)
                        )
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
