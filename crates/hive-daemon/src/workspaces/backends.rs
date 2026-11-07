//! Concrete node-local implementations. Durable lifecycle state remains in `Runtime`.

use super::*;
mod archive;
pub(super) mod jj;
mod vcs;
use hive_core::workspace::backend::{
    BackendData, BackendManager, CheckpointWriter, PreparationContext, PreparedWorkspace,
    ProvisionerCapabilities, ProvisionerDescriptor, ProvisionerId, RepositoryBackend,
    RepositoryBackendId, RepositoryCompatibility, RepositoryHandle, StorageKind,
    WorkspaceObservation, WorkspaceProvisioner,
};

pub(super) fn data<T: Serialize>(schema: &str, value: &T) -> Result<BackendData, Error> {
    Ok(BackendData {
        schema: schema.into(),
        version: 1,
        data: serde_json::to_value(value).map_err(|_| Error::Request)?,
    })
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct GitStorage {
    pub root: NodePath,
    pub source: String,
}

pub(super) fn handle(repository: &Repository) -> Result<RepositoryHandle, Error> {
    Ok(RepositoryHandle {
        id: repository.repository.id.clone(),
        project: repository.repository.project.clone(),
        node: repository.repository.node().clone(),
        backend: RepositoryBackendId("git".into()),
        storage: StorageKind("git.bare-cache".into()),
        metadata: data(
            "hive.git.repository",
            &GitStorage {
                root: repository.repository.root.clone(),
                source: repository.source.clone(),
            },
        )?,
    })
}

/// Compatibility decoding is confined to Git admission/inspection, not the lifecycle model.
pub(super) fn git_intent(entry: &Entry) -> Result<GitIntent, Error> {
    match &entry.workspace.intent().policy {
        WorkspacePolicy::Managed { intent, .. }
        | WorkspacePolicy::Ephemeral {
            backing: ManagedBacking::Backend { intent, .. },
        } => intent.decode("hive.git.intent").map_err(Into::into),
        WorkspacePolicy::Worktree { git, .. }
        | WorkspacePolicy::Clone { git }
        | WorkspacePolicy::Ephemeral {
            backing: ManagedBacking::Worktree { git, .. } | ManagedBacking::Clone { git },
        } => Ok(git.clone()),
        _ => Err(Error::Request),
    }
}

pub(super) fn intent(entry: &Entry) -> Result<BackendData, Error> {
    match &entry.workspace.intent().policy {
        WorkspacePolicy::Managed { intent, .. }
        | WorkspacePolicy::Ephemeral {
            backing: ManagedBacking::Backend { intent, .. },
        } => Ok(intent.clone()),
        WorkspacePolicy::Existing { directory } => data("hive.external.directory", directory),
        _ => data("hive.git.intent", &git_intent(entry)?),
    }
}

pub(super) fn supports_siblings(entry: &Entry) -> bool {
    entry.workspace.intent().ownership() == Ownership::HiveManaged
        && intent(entry).is_ok_and(|intent| {
            matches!(intent.schema.as_str(), "hive.git.intent" | "hive.jj.intent")
        })
}

pub(super) fn sibling_intent(
    entry: &Entry,
    base_ref: Option<GitRef>,
    branch: Option<GitRef>,
    revision: Option<hive_core::project::Revision>,
) -> Result<BackendData, Error> {
    let original = intent(entry)?;
    if original.schema == "hive.jj.intent" {
        if base_ref.is_some() || branch.is_some() {
            return Err(Error::Request);
        }
        let inherited: hive_core::project::Revision = original.decode("hive.jj.intent")?;
        data("hive.jj.intent", &revision.unwrap_or(inherited))
    } else {
        if revision.is_some() {
            return Err(Error::Request);
        }
        let inherited = git_intent(entry)?;
        // Reusing the parent's branch would alias its mutable native workspace identity.
        data(
            "hive.git.intent",
            &GitIntent {
                base_ref: base_ref.unwrap_or(inherited.base_ref),
                branch,
            },
        )
    }
}

pub(super) fn legacy_checkpoint(
    entry: &Entry,
    operation: &Operation,
    directory: &Path,
) -> Result<Option<BackendData>, Error> {
    if !operation.is_create() {
        return Ok(None);
    }
    if entry.workspace.intent().ownership() == Ownership::External {
        return intent(entry).map(Some);
    }
    let Some(commit) = &operation.commit else {
        return Ok(None);
    };
    let Some(path) = operation.path.clone().or_else(|| {
        entry
            .workspace
            .location()
            .map(|l| PathBuf::from(l.root.as_str()))
    }) else {
        if operation.effects_started
            && entry
                .selection
                .as_ref()
                .is_some_and(|selection| selection.provisioner.0 == "worktrunk")
        {
            // A lost WorkTrunk response has no observed path. Retain identity without inventing one.
            return data(
                "hive.git.pending-checkout",
                &PendingGitCheckout {
                    workspace: entry.workspace.id().clone(),
                    branch: git_intent(entry)?.effective_branch(entry.workspace.id()),
                    commit: commit.clone(),
                    operation: entry
                        .workspace
                        .operation()
                        .map(ToString::to_string)
                        .unwrap_or_default(),
                    state_directory: directory.into(),
                },
            )
            .map(Some);
        }
        return data("hive.git.commit", commit).map(Some);
    };
    let path = NodePath::new(
        entry.workspace.intent().node.clone(),
        if cfg!(windows) {
            PathSyntax::Windows
        } else {
            PathSyntax::Posix
        },
        path.to_str().ok_or(Error::Conflict)?.into(),
    )?;
    data(
        "hive.git.checkout",
        &GitCheckout {
            workspace: entry.workspace.id().clone(),
            path,
            branch: git_intent(entry)?.effective_branch(entry.workspace.id()),
            commit: commit.clone(),
            operation: entry
                .workspace
                .operation()
                .map(ToString::to_string)
                .unwrap_or_default(),
            state_directory: directory.into(),
            verify_base: false,
        },
    )
    .map(Some)
}

fn storage(repository: &RepositoryHandle) -> Result<GitStorage, Error> {
    if repository.backend.0 != "git" || repository.storage.0 != "git.bare-cache" {
        return Err(repository.unavailable().into());
    }
    let storage: GitStorage = repository.metadata.decode("hive.git.repository")?;
    if storage.root.node() != &repository.node {
        return Err(Error::Conflict);
    }
    Ok(storage)
}

pub(super) struct GitRepository;

impl RepositoryBackend for GitRepository {
    type Error = Error;

    fn supports_vcs(&self) -> bool {
        true
    }
    fn vcs(
        &self,
        target: hive_core::workspace::backend::vcs::Target<'_>,
        request: &hive_core::workspace::backend::vcs::Request,
    ) -> Result<hive_core::workspace::backend::vcs::Result, hive_core::workspace::backend::vcs::Error>
    {
        self.validate(target.repository)
            .map_err(|_| hive_core::workspace::backend::vcs::Error::Backend)?;
        vcs::execute(target, request, false)
    }

    fn validate(&self, repository: &RepositoryHandle) -> Result<(), Error> {
        storage(repository).map(|_| ())
    }

    fn verify(&self, repository: &RepositoryHandle) -> Result<(), Error> {
        let storage = storage(repository)?;
        let root = Path::new(storage.root.as_str());
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
            || git(root, &["config", "--get", "remote.origin.url"], "inspect")?.trim()
                != storage.source
            || git(root, &["config", "--get", "hive.repository"], "inspect")?.trim()
                != repository.id.as_str()
            || git(root, &["rev-parse", "--is-bare-repository"], "inspect")?.trim() != "true"
        {
            return Err(Error::Conflict);
        }
        Ok(())
    }

    fn acquire(&self, repository: &RepositoryHandle) -> Result<(), Error> {
        let storage = storage(repository)?;
        let destination = Path::new(storage.root.as_str());
        let parent = destination.parent().ok_or(Error::Request)?;
        std::fs::create_dir_all(parent).map_err(|_| Error::Storage)?;
        let staging = parent.join(".git-incomplete");
        // The durable repository ID owns this staging path; arbitrary occupied paths are never removed.
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
        // Bare and checkout-specific template settings must not leak to linked workspaces.
        let _ = git(
            &staging,
            &["config", "--local", "--unset-all", "core.bare"],
            "clone",
        );
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
        if common
            .split('\0')
            .any(|entry| entry.starts_with("core.bare\n") || entry.starts_with("core.worktree\n"))
        {
            return Err(Error::Git("clone"));
        }
        git(
            &staging,
            &["config", "remote.origin.url", &storage.source],
            "clone",
        )?;
        git(
            &staging,
            &["config", "hive.repository", repository.id.as_str()],
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
        // Explicit bases remain usable when a remote's default HEAD is unborn or detached.
        if let Some(branch) = head.lines().find_map(|line| {
            line.strip_prefix("ref: refs/heads/")
                .and_then(|rest| rest.split_once('\t'))
                .map(|(branch, _)| branch)
        }) {
            let target = format!("refs/remotes/origin/{branch}");
            git(
                &staging,
                &["symbolic-ref", "refs/remotes/origin/HEAD", &target],
                "clone",
            )?;
            // Only initial acquisition seeds local HEAD; later updates preserve checked-out branches.
            git(
                &staging,
                &["update-ref", &format!("refs/heads/{branch}"), &target],
                "clone",
            )?;
            git(
                &staging,
                &["symbolic-ref", "HEAD", &format!("refs/heads/{branch}")],
                "clone",
            )?;
        }
        std::fs::rename(&staging, destination).map_err(|_| Error::Storage)
    }

    fn update(&self, repository: &RepositoryHandle) -> Result<(), Error> {
        self.verify(repository)?;
        let storage = storage(repository)?;
        let root = Path::new(storage.root.as_str());
        git(
            root,
            &[
                "fetch",
                "--atomic",
                "--no-tags",
                "origin",
                "+refs/heads/*:refs/remotes/origin/*",
                "refs/tags/*:refs/tags/*",
            ],
            "fetch",
        )?;
        refresh_remote_head(root, "fetch")
    }

    fn inspect_revision(
        &self,
        repository: &RepositoryHandle,
        intent: &BackendData,
    ) -> Result<BackendData, Error> {
        self.verify(repository)?;
        let reference: GitRef = intent.decode("hive.git.ref")?;
        data(
            "hive.git.commit",
            &resolve(Path::new(storage(repository)?.root.as_str()), &reference)?,
        )
    }

    fn storage_layout(
        &self,
        repository: &RepositoryHandle,
    ) -> Result<hive_core::workspace::backend::RepositoryLayout, Error> {
        let storage = storage(repository)?;
        let parent = Path::new(storage.root.as_str())
            .parent()
            .ok_or(Error::Conflict)?;
        let node_path = |path: &Path| {
            NodePath::new(
                repository.node.clone(),
                storage.root.syntax(),
                path.to_str().ok_or(Error::Conflict)?.into(),
            )
            .map_err(Error::from)
        };
        Ok(hive_core::workspace::backend::RepositoryLayout {
            scan_roots: vec![node_path(parent)?],
            staging: vec![node_path(&parent.join(".git-incomplete"))?],
        })
    }

    fn workspace_paths(&self, repository: &RepositoryHandle) -> Result<Vec<NodePath>, Error> {
        self.verify(repository)?;
        let storage = storage(repository)?;
        let list = git(
            Path::new(storage.root.as_str()),
            &["worktree", "list", "--porcelain", "-z"],
            "inspect",
        )?;
        list.split('\0')
            .filter_map(|field| field.strip_prefix("worktree "))
            .filter(|path| *path != storage.root.as_str())
            .map(|path| {
                NodePath::new(repository.node.clone(), storage.root.syntax(), path.into())
                    .map_err(Into::into)
            })
            .collect()
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct GitCheckout {
    pub workspace: WorkspaceId,
    pub path: NodePath,
    pub branch: String,
    pub commit: String,
    pub operation: String,
    pub state_directory: PathBuf,
    #[serde(default)]
    pub verify_base: bool,
}

/// Legacy hook effects may exist without a returned path; only explicit reconciliation can bind it.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingGitCheckout {
    workspace: WorkspaceId,
    branch: String,
    commit: String,
    operation: String,
    state_directory: PathBuf,
}

#[derive(Clone, Copy)]
pub(super) enum GitProvisioner {
    Worktree,
    Clone,
    Worktrunk,
}

impl GitProvisioner {
    pub fn id(self) -> &'static str {
        match self {
            Self::Worktree => "git.worktree",
            Self::Clone => "git.clone",
            Self::Worktrunk => "worktrunk",
        }
    }

    fn observe(
        &self,
        repository: &RepositoryHandle,
        checkout: &GitCheckout,
        path: &Path,
    ) -> Result<WorkspaceObservation, Error> {
        let store = storage(repository)?;
        let top = git(path, &["rev-parse", "--show-toplevel"], "inspect")?;
        if git(path, &["config", "--get", "hive.workspace"], "inspect")?.trim()
            != checkout.workspace.as_str()
            || Path::new(top.trim())
                .canonicalize()
                .map_err(|_| Error::Conflict)?
                != path
            || git(path, &["symbolic-ref", "--short", "HEAD"], "inspect")?.trim() != checkout.branch
        {
            return Err(Error::Conflict);
        }
        if !matches!(self, Self::Clone) {
            GitRepository.verify(repository)?;
            let common = git(
                path,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
                "inspect",
            )?;
            if Path::new(common.trim())
                .canonicalize()
                .map_err(|_| Error::Conflict)?
                != Path::new(store.root.as_str())
                    .canonicalize()
                    .map_err(|_| Error::Conflict)?
            {
                return Err(Error::Conflict);
            }
        }
        let branch = git(path, &["symbolic-ref", "--short", "HEAD"], "inspect")?;
        let commit = git(path, &["rev-parse", "HEAD"], "inspect")?;
        if checkout.verify_base && commit.trim() != checkout.commit {
            // Hook-created commits are valid descendants; ordinary ready inspection permits rebases.
            git(
                path,
                &["merge-base", "--is-ancestor", &checkout.commit, "HEAD"],
                "base verification",
            )
            .map_err(|_| Error::Conflict)?;
        }
        let cleanliness = match git(
            path,
            &[
                // Scoped read tools also use this ownership observation; do not refresh its index.
                "--no-optional-locks",
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
        };
        let root = NodePath::new(
            repository.node.clone(),
            checkout.path.syntax(),
            path.to_str().ok_or(Error::Request)?.into(),
        )?;
        Ok(WorkspaceObservation {
            root: root.clone(),
            working_directory: root,
            metadata: data(
                "hive.git.observation",
                &serde_json::json!({"branch":branch.trim(),"commit":commit.trim()}),
            )?,
            cleanliness,
            ownership_verified: true,
        })
    }
}

/// Legacy response fields are projections of Git evidence, never inputs to generic dispatch.
pub(super) fn location(
    observation: WorkspaceObservation,
    backend: BackendId,
    pinned: Option<&str>,
) -> Result<WorkspaceLocation, Error> {
    let (resolved_base, branch, commit) = if observation.metadata.schema == "hive.git.observation" {
        let value: serde_json::Value = observation.metadata.decode("hive.git.observation")?;
        (
            pinned.map(|s| GitRef::try_from(s.to_owned())).transpose()?,
            Some(GitRef::try_from(
                value["branch"].as_str().ok_or(Error::Conflict)?.to_owned(),
            )?),
            Some(value["commit"].as_str().ok_or(Error::Conflict)?.to_owned()),
        )
    } else {
        (None, None, None)
    };
    Ok(WorkspaceLocation {
        root: observation.root,
        working_directory: observation.working_directory,
        observations: Some(observation.metadata),
        backend,
        resolved_base,
        branch,
        commit,
    })
}

impl WorkspaceProvisioner for GitProvisioner {
    type Error = Error;

    fn validate_intent(&self, intent: &BackendData) -> Result<(), Error> {
        match intent.schema.as_str() {
            "hive.git.intent" => intent
                .decode::<GitIntent>("hive.git.intent")
                .map(|_| ())
                .map_err(Into::into),
            "hive.git.checkout" => intent
                .decode::<GitCheckout>("hive.git.checkout")
                .map(|_| ())
                .map_err(Into::into),
            _ => Err(intent.unsupported().into()),
        }
    }

    fn descriptor(&self) -> ProvisionerDescriptor {
        ProvisionerDescriptor {
            id: ProvisionerId(self.id().into()),
            repositories: vec![RepositoryCompatibility {
                backend: RepositoryBackendId("git".into()),
                storage: StorageKind("git.bare-cache".into()),
            }],
            repository_optional: false,
            capabilities: ProvisionerCapabilities {
                linked_workspace: !matches!(self, Self::Clone),
                independent_checkout: matches!(self, Self::Clone),
                uncertain_external_effects: matches!(self, Self::Worktrunk),
                activity_reporting: matches!(self, Self::Worktrunk),
            },
        }
    }
    fn discovery_roots(&self, directory: &NodePath) -> Result<Vec<NodePath>, Error> {
        if !matches!(self, Self::Clone) {
            return Ok(vec![]);
        }
        Ok(vec![NodePath::new(
            directory.node().clone(),
            directory.syntax(),
            Path::new(directory.as_str())
                .join("clones")
                .to_str()
                .ok_or(Error::Request)?
                .into(),
        )?])
    }

    fn prepare(
        &self,
        repository: Option<&RepositoryHandle>,
        intent: &BackendData,
        context: PreparationContext<'_>,
        checkpoints: &mut dyn CheckpointWriter<Error = Error>,
    ) -> Result<PreparedWorkspace, Error> {
        let repository = repository.ok_or(Error::Conflict)?;
        let storage = storage(repository)?;
        let bare = Path::new(storage.root.as_str());
        let git_intent: GitIntent = intent.decode("hive.git.intent")?;
        let branch = git_intent.effective_branch(context.workspace);
        let commit = match context.previous {
            Some(previous) if previous.schema == "hive.git.checkout" => {
                previous.decode::<GitCheckout>("hive.git.checkout")?.commit
            }
            Some(previous) => previous.decode("hive.git.commit")?,
            None => resolve(bare, &git_intent.base_ref)?,
        };
        // Persist resolution even when branch validation fails; retries must never move the base.
        checkpoints.checkpoint(data("hive.git.commit", &commit)?)?;
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
        let target = if matches!(self, Self::Clone) {
            Path::new(context.directory.as_str())
                .join("clones")
                .join(context.workspace.as_str())
        } else {
            bare.parent()
                .ok_or(Error::Conflict)?
                .join(context.workspace.as_str())
        };
        if !matches!(self, Self::Worktrunk) && target.try_exists().map_err(|_| Error::Conflict)? {
            return Err(Error::Conflict);
        }
        let path = NodePath::new(
            repository.node.clone(),
            context.directory.syntax(),
            target.to_str().ok_or(Error::Request)?.into(),
        )?;
        let checkout = GitCheckout {
            workspace: context.workspace.clone(),
            path: path.clone(),
            branch,
            commit,
            operation: context.operation.to_string(),
            state_directory: PathBuf::from(context.directory.as_str()),
            verify_base: true,
        };
        Ok(PreparedWorkspace {
            intent: data("hive.git.checkout", &checkout)?,
            path: (!matches!(self, Self::Worktrunk)).then_some(path),
        })
    }

    fn at_location(
        &self,
        intent: &BackendData,
        path: &NodePath,
        verify_creation: bool,
    ) -> Result<BackendData, Error> {
        let mut checkout: GitCheckout = if intent.schema == "hive.git.pending-checkout"
            && matches!(self, Self::Worktrunk)
            && verify_creation
        {
            let pending: PendingGitCheckout = intent.decode("hive.git.pending-checkout")?;
            GitCheckout {
                workspace: pending.workspace,
                path: path.clone(),
                branch: pending.branch,
                commit: pending.commit,
                operation: pending.operation,
                state_directory: pending.state_directory,
                verify_base: true,
            }
        } else {
            intent.decode("hive.git.checkout")?
        };
        if verify_creation
            && !matches!(self, Self::Worktrunk)
            && Path::new(checkout.path.as_str())
                .canonicalize()
                .map_err(|_| Error::Conflict)?
                != Path::new(path.as_str())
                    .canonicalize()
                    .map_err(|_| Error::Conflict)?
        {
            return Err(Error::Conflict);
        }
        checkout.path = path.clone();
        checkout.verify_base = verify_creation;
        data("hive.git.checkout", &checkout)
    }

    fn create(
        &self,
        repository: Option<&RepositoryHandle>,
        intent: &BackendData,
        checkpoints: &mut dyn CheckpointWriter<Error = Error>,
    ) -> Result<WorkspaceObservation, Error> {
        let repository = repository.ok_or(Error::Conflict)?;
        let store = storage(repository)?;
        let checkout: GitCheckout = intent.decode("hive.git.checkout")?;
        let bare = Path::new(store.root.as_str());
        let target = Path::new(checkout.path.as_str());
        let actual = match self {
            Self::Worktrunk => {
                let configured = std::env::var_os("WORKTRUNK_PROJECT_CONFIG_PATH")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from(".config/wt.toml"));
                let config = if configured.is_absolute() {
                    Some(configured)
                } else {
                    match git(
                        bare,
                        &[
                            "show",
                            &format!(
                                "{}:{}",
                                checkout.commit,
                                configured.to_str().ok_or(Error::Request)?
                            ),
                        ],
                        "hook configuration",
                    ) {
                        Ok(contents) => {
                            let directory = checkout.state_directory.join("worktrunk-config");
                            std::fs::create_dir_all(&directory).map_err(|_| Error::Storage)?;
                            let path = directory.join(format!("{}.toml", checkout.operation));
                            std::fs::write(&path, contents).map_err(|_| Error::Storage)?;
                            Some(path)
                        }
                        Err(Error::Git(_)) => None,
                        Err(error) => return Err(error),
                    }
                };
                check_approvals_with_config(bare, config.as_deref())?;
                // A durable barrier must precede hooks: their lost responses cannot be replayed safely.
                checkpoints.checkpoint(data("hive.workspace.effects", &true)?)?;
                let output = wt_with_config(
                    bare,
                    &[
                        "switch",
                        "--create",
                        "--base",
                        &checkout.commit,
                        "--no-cd",
                        "--format",
                        "json",
                        &checkout.branch,
                    ],
                    config.as_deref(),
                )?;
                let value: serde_json::Value =
                    serde_json::from_str(&output).map_err(|_| Error::Uncertain)?;
                if value["action"] != "created"
                    || value["created_branch"] != true
                    || value["branch"] != checkout.branch
                {
                    return Err(Error::Uncertain);
                }
                PathBuf::from(value["path"].as_str().ok_or(Error::Uncertain)?)
            }
            Self::Clone => {
                std::fs::create_dir_all(target.parent().ok_or(Error::Request)?)
                    .map_err(|_| Error::Storage)?;
                git(
                    bare,
                    &[
                        "clone",
                        "--no-hardlinks",
                        "--no-checkout",
                        bare.to_str().ok_or(Error::Request)?,
                        checkout.path.as_str(),
                    ],
                    "clone",
                )?;
                git(
                    target,
                    &["remote", "set-url", "origin", &store.source],
                    "clone",
                )?;
                #[cfg(windows)]
                git(target, &["config", "core.longpaths", "true"], "clone")?;
                git(
                    target,
                    &["checkout", "-b", &checkout.branch, &checkout.commit],
                    "checkout",
                )?;
                target.to_path_buf()
            }
            Self::Worktree => {
                git(
                    bare,
                    &[
                        "worktree",
                        "add",
                        "-b",
                        &checkout.branch,
                        checkout.path.as_str(),
                        &checkout.commit,
                    ],
                    "worktree creation",
                )?;
                target.to_path_buf()
            }
        };
        if !actual.is_absolute() {
            return Err(Error::Uncertain);
        }
        let actual = actual.canonicalize().map_err(|_| Error::Conflict)?;
        git(
            &actual,
            &[
                "config",
                if matches!(self, Self::Clone) {
                    "--local"
                } else {
                    "--worktree"
                },
                "hive.workspace",
                checkout.workspace.as_str(),
            ],
            "ownership",
        )?;
        // Publish the actual returned path before further inspection can fail.
        checkpoints.checkpoint(data("hive.workspace.path", &actual)?)?;
        self.observe(repository, &checkout, &actual)
    }

    fn inspect(
        &self,
        repository: Option<&RepositoryHandle>,
        intent: &BackendData,
    ) -> Result<WorkspaceObservation, Error> {
        let checkout: GitCheckout = intent.decode("hive.git.checkout")?;
        self.observe(
            repository.ok_or(Error::Conflict)?,
            &checkout,
            Path::new(checkout.path.as_str()),
        )
    }

    fn verify_created(
        &self,
        repository: Option<&RepositoryHandle>,
        intent: &BackendData,
    ) -> Result<WorkspaceObservation, Error> {
        let observation = self.inspect(repository, intent)?;
        let store = storage(repository.ok_or(Error::Conflict)?)?;
        let path = Path::new(observation.root.as_str());
        if matches!(self, Self::Clone) {
            if git(path, &["config", "--get", "remote.origin.url"], "inspect")?.trim()
                != store.source
            {
                return Err(Error::Conflict);
            }
        } else {
            let registrations = git(
                Path::new(store.root.as_str()),
                &["worktree", "list", "--porcelain", "-z"],
                "inspect",
            )?;
            if !registered_worktree(&registrations, path)? {
                return Err(Error::Conflict);
            }
        }
        Ok(observation)
    }

    fn is_absent(
        &self,
        repository: Option<&RepositoryHandle>,
        intent: &BackendData,
    ) -> Result<bool, Error> {
        let checkout: GitCheckout = intent.decode("hive.git.checkout")?;
        let path = Path::new(checkout.path.as_str());
        match path.symlink_metadata() {
            Ok(_) => return Ok(false),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(Error::Conflict),
        }
        if matches!(self, Self::Clone) {
            return Ok(true);
        }
        let repository = repository.ok_or(Error::Conflict)?;
        GitRepository.verify(repository)?;
        let store = storage(repository)?;
        let registrations = git(
            Path::new(store.root.as_str()),
            &["worktree", "list", "--porcelain", "-z"],
            "inspect",
        )?;
        Ok(!registered_worktree(&registrations, path)?)
    }

    fn prepare_cleanup(
        &self,
        _repository: Option<&RepositoryHandle>,
        intent: &BackendData,
    ) -> Result<(), Error> {
        if matches!(self, Self::Worktrunk) {
            let checkout: GitCheckout = intent.decode("hive.git.checkout")?;
            check_approvals(Path::new(checkout.path.as_str()))?;
        }
        Ok(())
    }

    fn preserve(
        &self,
        _repository: Option<&RepositoryHandle>,
        intent: &BackendData,
        context: PreparationContext<'_>,
        checkpoints: &mut dyn CheckpointWriter<Error = Error>,
    ) -> Result<Option<NodePath>, Error> {
        if !matches!(self, Self::Clone) {
            return Ok(None);
        }
        let checkout: GitCheckout = intent.decode("hive.git.checkout")?;
        archive::preserve(&checkout.path, context, checkpoints).map(Some)
    }

    fn verify_preserved(
        &self,
        archive: Option<&NodePath>,
        checkpoint: Option<&BackendData>,
        operation: &OperationId,
    ) -> Result<bool, Error> {
        if !matches!(self, Self::Clone) {
            return Ok(true);
        }
        let (Some(archive), Some(checkpoint)) = (archive, checkpoint) else {
            return Ok(false);
        };
        let plan: ArchivePlan = checkpoint.decode("hive.git.archive")?;
        Ok(Path::new(archive.as_str()) == plan.destination && archive::verified(&plan, operation))
    }

    fn reconcile(
        &self,
        repository: Option<&RepositoryHandle>,
        intent: &BackendData,
    ) -> Result<WorkspaceObservation, Error> {
        let repository = repository.ok_or(Error::Conflict)?;
        GitRepository.verify(repository)?;
        let store = storage(repository)?;
        let checkout: GitCheckout = intent.decode("hive.git.checkout")?;
        let path = Path::new(checkout.path.as_str());
        if matches!(self, Self::Clone) {
            if git(path, &["config", "--get", "remote.origin.url"], "inspect")?.trim()
                != store.source
            {
                return Err(Error::Conflict);
            }
        } else {
            let common = git(
                path,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
                "inspect",
            )?;
            if Path::new(common.trim())
                .canonicalize()
                .map_err(|_| Error::Conflict)?
                != Path::new(store.root.as_str())
                    .canonicalize()
                    .map_err(|_| Error::Conflict)?
            {
                return Err(Error::Conflict);
            }
            let list = git(
                Path::new(store.root.as_str()),
                &["worktree", "list", "--porcelain", "-z"],
                "inspect",
            )?;
            if !registered_worktree(&list, path)? {
                return Err(Error::Conflict);
            }
        }
        if git(path, &["rev-parse", "HEAD"], "inspect")?.trim() != checkout.commit {
            git(
                path,
                &["merge-base", "--is-ancestor", &checkout.commit, "HEAD"],
                "base verification",
            )
            .map_err(|_| Error::Conflict)?;
        }
        if git(path, &["symbolic-ref", "--short", "HEAD"], "inspect")?.trim() != checkout.branch {
            return Err(Error::Conflict);
        }
        if let Ok(marker) = git(path, &["config", "--get", "hive.workspace"], "inspect")
            && marker.trim() != checkout.workspace.as_str()
        {
            return Err(Error::Conflict);
        }
        // Only explicit reconciliation claims a checkout after proving registration, branch and base.
        git(
            path,
            &[
                "config",
                if matches!(self, Self::Clone) {
                    "--local"
                } else {
                    "--worktree"
                },
                "hive.workspace",
                checkout.workspace.as_str(),
            ],
            "ownership",
        )?;
        self.observe(repository, &checkout, path)
    }

    fn remove(
        &self,
        repository: Option<&RepositoryHandle>,
        intent: &BackendData,
        discard: bool,
    ) -> Result<(), Error> {
        let checkout: GitCheckout = intent.decode("hive.git.checkout")?;
        let path = Path::new(checkout.path.as_str());
        let store = storage(repository.ok_or(Error::Conflict)?)?;
        let bare = Path::new(store.root.as_str());
        match self {
            Self::Clone => std::fs::remove_dir_all(path).map_err(|_| Error::Removal),
            Self::Worktree => {
                let mut args = vec!["worktree", "remove"];
                if discard {
                    args.push("--force");
                }
                args.push(checkout.path.as_str());
                git(bare, &args, "worktree removal").map(|_| ())
            }
            Self::Worktrunk => {
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
                args.push(checkout.path.as_str());
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
            }
        }
    }
}

pub(super) fn manager() -> BackendManager<Error> {
    let mut registry = BackendManager::default();
    registry
        .register_repository(
            RepositoryCompatibility {
                backend: RepositoryBackendId("jj".into()),
                storage: StorageKind("jj.git".into()),
            },
            Arc::new(jj::Jj),
        )
        .expect("unique JJ repository identity");
    registry
        .register_provisioner(Arc::new(jj::Jj))
        .expect("unique JJ provisioner identity");
    registry
        .register_repository(
            RepositoryCompatibility {
                backend: RepositoryBackendId("git".into()),
                storage: StorageKind("git.bare-cache".into()),
            },
            Arc::new(GitRepository),
        )
        .expect("unique built-in repository identity");
    for provisioner in [
        GitProvisioner::Worktree,
        GitProvisioner::Clone,
        GitProvisioner::Worktrunk,
    ] {
        registry
            .register_provisioner(Arc::new(provisioner))
            .expect("unique built-in provisioner identity");
    }
    registry
        .register_provisioner(Arc::new(ExternalDirectory))
        .expect("unique external provisioner identity");
    registry
}

pub(super) struct ExternalDirectory;

impl WorkspaceProvisioner for ExternalDirectory {
    type Error = Error;
    fn validate_intent(&self, intent: &BackendData) -> Result<(), Error> {
        intent
            .decode::<NodePath>("hive.external.directory")
            .map(|_| ())
            .map_err(Into::into)
    }
    fn descriptor(&self) -> ProvisionerDescriptor {
        ProvisionerDescriptor {
            id: ProvisionerId("external.directory".into()),
            repositories: vec![],
            repository_optional: true,
            capabilities: ProvisionerCapabilities::default(),
        }
    }
    fn at_location(
        &self,
        _intent: &BackendData,
        path: &NodePath,
        _verify_creation: bool,
    ) -> Result<BackendData, Error> {
        data("hive.external.directory", path)
    }
    fn create(
        &self,
        repository: Option<&RepositoryHandle>,
        intent: &BackendData,
        checkpoints: &mut dyn CheckpointWriter<Error = Error>,
    ) -> Result<WorkspaceObservation, Error> {
        let observation = self.inspect(repository, intent)?;
        checkpoints.checkpoint(data(
            "hive.workspace.path",
            &PathBuf::from(observation.root.as_str()),
        )?)?;
        Ok(observation)
    }
    fn inspect(
        &self,
        repository: Option<&RepositoryHandle>,
        intent: &BackendData,
    ) -> Result<WorkspaceObservation, Error> {
        if repository.is_some() {
            return Err(hive_core::workspace::WorkspaceError::IncompatibleBackends {
                provisioner: "external.directory".into(),
                backend: repository.map(|r| r.backend.0.clone()),
                storage: repository.map(|r| r.storage.0.clone()),
            }
            .into());
        }
        let root: NodePath = intent.decode("hive.external.directory")?;
        if !Path::new(root.as_str()).is_dir() {
            return Err(Error::Conflict);
        }
        Ok(WorkspaceObservation {
            root: root.clone(),
            working_directory: root,
            metadata: data("hive.external.observation", &())?,
            cleanliness: Cleanliness::Unknown,
            ownership_verified: false,
        })
    }
    fn remove(
        &self,
        _repository: Option<&RepositoryHandle>,
        _intent: &BackendData,
        _discard: bool,
    ) -> Result<(), Error> {
        // Even a caller bypassing orchestration cannot turn an external directory into managed data.
        Err(Error::Request)
    }
    fn is_absent(
        &self,
        _repository: Option<&RepositoryHandle>,
        _intent: &BackendData,
    ) -> Result<bool, Error> {
        // External records can be detached, but managed cleanup cannot prove ownership of them.
        Ok(false)
    }
}
