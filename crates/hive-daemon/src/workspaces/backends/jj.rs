//! Native non-colocated JJ storage; the shared service owns lifecycle and usage policy.
use super::*;
use hive_core::project::Revision;
use std::io::Write;

pub(super) struct Jj;

pub(in crate::workspaces) fn storage_key(id: &RepositoryId) -> Result<String, Error> {
    let mut value = uuid::Uuid::parse_str(
        id.as_str()
            .strip_prefix("repository-")
            .ok_or(Error::Conflict)?,
    )
    .map_err(|_| Error::Conflict)?
    .as_u128();
    // JJ appends 128-character index names. Encode all UUID bits in case-insensitive
    // base32 rather than truncating identity or exhausting Windows' legacy path limit.
    let mut key = [b'0'; 26];
    for digit in key.iter_mut().rev() {
        *digit = b"0123456789abcdefghijklmnopqrstuv"[(value & 31) as usize];
        value >>= 5;
    }
    String::from_utf8(key.to_vec()).map_err(|_| Error::Storage)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Storage {
    root: NodePath,
    staging: NodePath,
    source: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Checkout {
    workspace: WorkspaceId,
    repository: RepositoryId,
    operation: OperationId,
    path: NodePath,
    name: String,
    revision: Revision,
    base_commit: String,
    base_change: String,
}

#[derive(Deserialize)]
struct Registration(String, String, String, String);

#[derive(Deserialize)]
struct Commit(String, String, bool, bool);

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Removal {
    repository: RepositoryHandle,
    checkout: Checkout,
    operation: OperationId,
    commit: String,
    bookmark: String,
}

fn retained(removal: &Removal) -> Result<bool, Error> {
    RepositoryBackend::verify(&Jj, &removal.repository)?;
    let store = storage(&removal.repository)?;
    let revisions = commits(
        Path::new(store.root.as_str()),
        &format!("bookmarks(exact:{})", removal.bookmark),
    )?;
    Ok(revisions.len() == 1 && revisions[0].0 == removal.commit)
}

const COMMITS: &str = "concat(\"[\", json(commit_id), \",\", json(change_id), \",\", json(empty), \",\", json(conflict), \"]\\n\")";
const WORKSPACES: &str = "concat(\"[\", json(name), \",\", json(root), \",\", json(target.commit_id()), \",\", json(target.change_id()), \"]\\n\")";
const FILES: &str = "concat(\"[\", json(path), \",\", json(file_type), \",\", json(executable), \",\", json(stringify(normal_value)), \"]\\n\")";

pub(in crate::workspaces) fn handle(repository: &Repository) -> Result<RepositoryHandle, Error> {
    Ok(RepositoryHandle {
        id: repository.repository.id.clone(),
        project: repository.repository.project.clone(),
        node: repository.repository.node().clone(),
        backend: RepositoryBackendId("jj".into()),
        storage: StorageKind("jj.git".into()),
        metadata: data(
            "hive.jj.repository",
            &Storage {
                root: repository.repository.root.clone(),
                staging: NodePath::new(
                    repository.repository.node().clone(),
                    repository.repository.root.syntax(),
                    Path::new(repository.repository.root.as_str())
                        .with_file_name("t")
                        .to_str()
                        .ok_or(Error::Request)?
                        .into(),
                )?,
                source: repository.source.clone(),
            },
        )?,
    })
}

fn storage(repository: &RepositoryHandle) -> Result<Storage, Error> {
    if repository.backend.0 != "jj" || repository.storage.0 != "jj.git" {
        return Err(repository.unavailable().into());
    }
    let store: Storage = repository.metadata.decode("hive.jj.repository")?;
    if store.root.node() != &repository.node
        || store.staging.node() != &repository.node
        || store.root == store.staging
    {
        return Err(Error::Conflict);
    }
    Ok(store)
}

pub(super) fn run(root: &Path, args: &[&str]) -> Result<String, Error> {
    String::from_utf8(run_bytes(root, args)?).map_err(|_| Error::Uncertain)
}

pub(super) fn run_bytes(root: &Path, args: &[&str]) -> Result<Vec<u8>, Error> {
    run_output(root, args, false)
}

pub(super) fn run_output(root: &Path, args: &[&str], digest: bool) -> Result<Vec<u8>, Error> {
    // Never snapshot an agent's files as a side effect of inspection or repository maintenance.
    let mut argv = vec![
        "--no-pager",
        "--color=never",
        "--config=ui.progress-indicator=false",
        "--config=signing.behavior=drop",
    ];
    // JJ also suppresses materialization for *new* workspaces with this flag.
    // Only acquisition/creation may materialize; inspections never snapshot agent files.
    if !args.starts_with(&["git", "clone"]) && !args.starts_with(&["workspace", "add"]) {
        argv.push("--ignore-working-copy");
    }
    argv.extend_from_slice(args);
    execute_output_mode("jj", Some(root), &argv, false, None, digest).map_err(|error| match error {
        Error::Jj(_) => Error::Jj(if args.starts_with(&["git", "clone"]) {
            "repository clone"
        } else if args.starts_with(&["git", "fetch"]) {
            "repository fetch"
        } else if args.starts_with(&["workspace", "add"]) {
            "workspace creation"
        } else {
            "repository inspection or maintenance"
        }),
        other => other,
    })
}

fn capabilities() -> Result<(), Error> {
    let version = execute("jj", None, &["--version"], false).map_err(|_| Error::JjUnavailable)?;
    let version = version
        .split_whitespace()
        .nth(1)
        .ok_or(Error::JjUnavailable)?;
    let mut numbers = version.split('.');
    let major: u32 = numbers
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or(Error::JjUnavailable)?;
    let minor: u32 = numbers
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or(Error::JjUnavailable)?;
    if major == 0 && minor < 46 {
        return Err(Error::JjUnavailable);
    }
    for args in [["workspace", "add", "--help"], ["git", "clone", "--help"]] {
        let help = execute("jj", None, &args, false).map_err(|_| Error::JjUnavailable)?;
        if !help.contains("--no-colocate") {
            return Err(Error::JjUnavailable);
        }
    }
    Ok(())
}

fn lines<T: serde::de::DeserializeOwned>(text: &str) -> Result<Vec<T>, Error> {
    text.lines()
        .map(|line| serde_json::from_str(line).map_err(|_| Error::Uncertain))
        .collect()
}

fn commits(root: &Path, revision: &str) -> Result<Vec<Commit>, Error> {
    // An equals-bound option value cannot be reinterpreted as a CLI flag.
    lines(&run(
        root,
        &[
            "--no-integrate-operation",
            "log",
            "--no-graph",
            &format!("--revisions={revision}"),
            "--template",
            COMMITS,
        ],
    )?)
}

fn registrations(root: &Path) -> Result<Vec<Registration>, Error> {
    lines(&run(
        root,
        &[
            "--no-integrate-operation",
            "workspace",
            "list",
            "--template",
            WORKSPACES,
        ],
    )?)
}

fn native_repo(root: &Path) -> Result<PathBuf, Error> {
    for path in [root.to_path_buf(), root.join(".jj"), root.join(".jj/repo")] {
        if std::fs::symlink_metadata(&path)
            .map_err(|_| Error::Conflict)?
            .file_type()
            .is_symlink()
        {
            return Err(Error::Conflict);
        }
    }
    let repo = root.join(".jj/repo");
    let repo = if repo.is_file() {
        let link = std::fs::read_to_string(repo).map_err(|_| Error::Conflict)?;
        root.join(".jj").join(link.trim())
    } else {
        repo
    };
    repo.canonicalize().map_err(|_| Error::Conflict)
}

fn write_marker<T: Serialize>(path: &Path, value: &T) -> Result<(), Error> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|_| Error::Conflict)?;
    file.write_all(&serde_json::to_vec(value).map_err(|_| Error::Storage)?)
        .map_err(|_| Error::Storage)?;
    // Recovery must never see a published resource whose ownership proof only lived in a buffer.
    file.sync_all().map_err(|_| Error::Storage)
}

fn read_marker<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, Error> {
    let metadata = std::fs::symlink_metadata(path).map_err(|_| Error::Conflict)?;
    if !metadata.is_file() || metadata.len() > 65536 {
        return Err(Error::Conflict);
    }
    serde_json::from_slice(&std::fs::read(path).map_err(|_| Error::Conflict)?)
        .map_err(|_| Error::Conflict)
}

impl RepositoryBackend for Jj {
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
        super::vcs::execute(target, request, true)
    }
    fn validate(&self, repository: &RepositoryHandle) -> Result<(), Error> {
        storage(repository).map(|_| ())
    }

    fn verify(&self, repository: &RepositoryHandle) -> Result<(), Error> {
        capabilities()?;
        let store = storage(repository)?;
        let root = Path::new(store.root.as_str());
        native_repo(root)?;
        if root.join(".git").exists() {
            return Err(Error::Conflict);
        }
        let marker: RepositoryHandle = read_marker(&root.join(".jj/hive-repository.json"))?;
        if &marker != repository {
            return Err(Error::Conflict);
        }
        // JJ lists rewritten URLs; compare the stored locator without expanding node credentials.
        // `git root` is JJ's explicit Git interoperability boundary, not a generic storage assumption.
        let git_root = run(root, &["--no-integrate-operation", "git", "root"])?;
        if !Path::new(git_root.trim())
            .canonicalize()
            .map_err(|_| Error::Conflict)?
            .starts_with(native_repo(root)?)
        {
            return Err(Error::Conflict);
        }
        let source = execute(
            "git",
            Some(root),
            &[
                "--git-dir",
                git_root.trim(),
                "config",
                "--local",
                "--get",
                "remote.origin.url",
            ],
            false,
        )?;
        if source.trim() != store.source {
            return Err(Error::Conflict);
        }
        // Template/flag probes also reject installations with incompatible capabilities.
        registrations(root)
            .map(|_| ())
            .map_err(|_| Error::JjUnavailable)
    }

    fn acquire(&self, repository: &RepositoryHandle) -> Result<(), Error> {
        capabilities()?;
        let store = storage(repository)?;
        let root = Path::new(store.root.as_str());
        if root.try_exists().map_err(|_| Error::Conflict)? {
            return RepositoryBackend::verify(self, repository);
        }
        let staging = Path::new(store.staging.as_str());
        // Short staging components leave room for JJ's content-addressed index names on Windows.
        let candidate = staging.join("r");
        let parent = root.parent().ok_or(Error::Conflict)?;
        std::fs::create_dir_all(parent).map_err(|_| Error::Storage)?;
        if staging.try_exists().map_err(|_| Error::Conflict)? {
            if !std::fs::symlink_metadata(staging)
                .map_err(|_| Error::Conflict)?
                .is_dir()
            {
                return Err(Error::Conflict);
            }
            let marker: RepositoryHandle = read_marker(&staging.join("hive-acquisition.json"))?;
            if &marker != repository {
                return Err(Error::Conflict);
            }
        } else {
            std::fs::create_dir(staging).map_err(|_| Error::Storage)?;
            // Reserve staging ownership before clone so a lost response has an inspectable identity.
            write_marker(&staging.join("hive-acquisition.json"), repository)?;
        }
        if !candidate.try_exists().map_err(|_| Error::Conflict)? {
            run(
                parent,
                &[
                    "git",
                    "clone",
                    "--no-colocate",
                    "--remote",
                    "origin",
                    "--",
                    &store.source,
                    candidate.to_str().ok_or(Error::Request)?,
                ],
            )?;
        }
        native_repo(&candidate)?;
        let git_root = run(&candidate, &["--no-integrate-operation", "git", "root"])?;
        let source = execute(
            "git",
            Some(&candidate),
            &[
                "--git-dir",
                git_root.trim(),
                "config",
                "--local",
                "--get",
                "remote.origin.url",
            ],
            false,
        )?;
        if source.trim() != store.source || candidate.join(".git").exists() {
            return Err(Error::Conflict);
        }
        let marker = candidate.join(".jj/hive-repository.json");
        if !marker.try_exists().map_err(|_| Error::Conflict)? {
            write_marker(&marker, repository)?;
        }
        let observed: RepositoryHandle = read_marker(&marker)?;
        if &observed != repository {
            return Err(Error::Conflict);
        }
        std::fs::rename(&candidate, root).map_err(|_| Error::Storage)?;
        // The container only carries Hive's acquisition marker; never recursively erase leftovers.
        std::fs::remove_file(staging.join("hive-acquisition.json")).map_err(|_| Error::Storage)?;
        std::fs::remove_dir(staging).map_err(|_| Error::Storage)?;
        RepositoryBackend::verify(self, repository)
    }

    fn update(&self, repository: &RepositoryHandle) -> Result<(), Error> {
        RepositoryBackend::verify(self, repository)?;
        run(
            Path::new(storage(repository)?.root.as_str()),
            &["git", "fetch", "--remote", "origin"],
        )
        .map(|_| ())
    }

    fn inspect_revision(
        &self,
        repository: &RepositoryHandle,
        intent: &BackendData,
    ) -> Result<BackendData, Error> {
        RepositoryBackend::verify(self, repository)?;
        let revision: Revision = intent.decode("hive.jj.intent")?;
        let result = commits(
            Path::new(storage(repository)?.root.as_str()),
            revision.as_str(),
        )
        .map_err(|error| match error {
            Error::Jj(_) => Error::JjRevision,
            other => other,
        })?;
        let [Commit(commit, change, _, conflict)] = result.as_slice() else {
            return Err(Error::JjRevision);
        };
        if *conflict {
            return Err(Error::JjRevision);
        }
        data("hive.jj.revision", &(commit, change))
    }

    fn storage_layout(
        &self,
        repository: &RepositoryHandle,
    ) -> Result<hive_core::workspace::backend::RepositoryLayout, Error> {
        let store = storage(repository)?;
        let root = Path::new(store.root.as_str());
        Ok(hive_core::workspace::backend::RepositoryLayout {
            scan_roots: vec![node_path(
                repository,
                root.parent().ok_or(Error::Conflict)?,
            )?],
            staging: vec![store.staging.clone()],
        })
    }
    fn workspace_paths(&self, repository: &RepositoryHandle) -> Result<Vec<NodePath>, Error> {
        RepositoryBackend::verify(self, repository)?;
        registrations(Path::new(storage(repository)?.root.as_str()))?
            .into_iter()
            .map(|r| node_path(repository, Path::new(&r.1)))
            .collect()
    }
}

fn node_path(repository: &RepositoryHandle, path: &Path) -> Result<NodePath, Error> {
    let store = storage(repository)?;
    Ok(NodePath::new(
        repository.node.clone(),
        store.root.syntax(),
        path.to_str().ok_or(Error::Request)?.into(),
    )?)
}

fn same_path(actual: &str, expected: &NodePath) -> Result<bool, Error> {
    // Resolve existing parents even after deletion, including Windows extended/native spellings.
    Ok(
        physical_path(Path::new(actual)).map_err(|_| Error::Conflict)?
            == physical_path(Path::new(expected.as_str())).map_err(|_| Error::Conflict)?,
    )
}

impl Jj {
    fn identity(
        &self,
        repository: &RepositoryHandle,
        checkout: &Checkout,
        marker: bool,
    ) -> Result<Registration, Error> {
        RepositoryBackend::verify(self, repository)?;
        if checkout.repository != repository.id || checkout.path.node() != &repository.node {
            return Err(Error::Conflict);
        }
        let store = storage(repository)?;
        let root = Path::new(checkout.path.as_str());
        if root.canonicalize().map_err(|_| Error::Conflict)? != root
            || native_repo(root)? != native_repo(Path::new(store.root.as_str()))?
        {
            return Err(Error::Conflict);
        }
        let registration = registrations(Path::new(store.root.as_str()))?
            .into_iter()
            .find(|r| r.0 == checkout.name)
            .ok_or(Error::Conflict)?;
        if Path::new(&registration.1)
            .canonicalize()
            .map_err(|_| Error::Conflict)?
            != root
        {
            return Err(Error::Conflict);
        }
        let current = commits(root, "@")?;
        if current.len() != 1 || current[0].0 != registration.2 {
            return Err(Error::Conflict);
        }
        if marker {
            let observed: Checkout = read_marker(&root.join(".jj/hive-workspace.json"))?;
            if &observed != checkout {
                return Err(Error::Conflict);
            }
        }
        Ok(registration)
    }

    fn observe(
        &self,
        repository: &RepositoryHandle,
        checkout: &Checkout,
    ) -> Result<WorkspaceObservation, Error> {
        let registration = self.identity(repository, checkout, true)?;
        let root = Path::new(checkout.path.as_str());
        let cleanliness = clean(root).unwrap_or(Cleanliness::Unknown);
        // Native operation identity complements the stable Hive create-operation identity.
        let native_operation = run(
            root,
            &[
                "--no-integrate-operation",
                "operation",
                "log",
                "--limit",
                "1",
                "--no-graph",
                "--template",
                "id",
            ],
        )?;
        Ok(WorkspaceObservation {
            root: checkout.path.clone(),
            working_directory: checkout.path.clone(),
            metadata: data(
                "hive.jj.observation",
                &serde_json::json!({
                    "workspace": registration.0, "repository": repository.id, "operation": checkout.operation,
                    "base_commit": checkout.base_commit, "base_change": checkout.base_change,
                    "commit": registration.2, "change": registration.3, "native_operation": native_operation,
                }),
            )?,
            cleanliness,
            ownership_verified: true,
        })
    }
}

impl WorkspaceProvisioner for Jj {
    type Error = Error;
    fn descriptor(&self) -> ProvisionerDescriptor {
        ProvisionerDescriptor {
            id: ProvisionerId("jj.workspace".into()),
            repositories: vec![RepositoryCompatibility {
                backend: RepositoryBackendId("jj".into()),
                storage: StorageKind("jj.git".into()),
            }],
            repository_optional: false,
            capabilities: ProvisionerCapabilities {
                linked_workspace: true,
                ..Default::default()
            },
        }
    }
    fn validate_intent(&self, intent: &BackendData) -> Result<(), Error> {
        match intent.schema.as_str() {
            "hive.jj.intent" => intent
                .decode::<Revision>("hive.jj.intent")
                .map(|_| ())
                .map_err(Into::into),
            "hive.jj.checkout" => intent
                .decode::<Checkout>("hive.jj.checkout")
                .map(|_| ())
                .map_err(Into::into),
            _ => Err(intent.unsupported().into()),
        }
    }
    fn prepare(
        &self,
        repository: Option<&RepositoryHandle>,
        intent: &BackendData,
        context: PreparationContext<'_>,
        checkpoints: &mut dyn CheckpointWriter<Error = Error>,
    ) -> Result<PreparedWorkspace, Error> {
        let repository = repository.ok_or(Error::Conflict)?;
        let revision: Revision = intent.decode("hive.jj.intent")?;
        let (commit, change) = match context.previous {
            Some(previous) if previous.schema == "hive.jj.checkout" => {
                let checkout: Checkout = previous.decode("hive.jj.checkout")?;
                (checkout.base_commit, checkout.base_change)
            }
            Some(previous) => previous.decode("hive.jj.revision")?,
            None => self
                .inspect_revision(repository, intent)?
                .decode("hive.jj.revision")?,
        };
        checkpoints.checkpoint(data("hive.jj.revision", &(&commit, &change))?)?;
        let store = storage(repository)?;
        let path = Path::new(store.root.as_str())
            .parent()
            .ok_or(Error::Conflict)?
            .join(context.workspace.as_str());
        if path.try_exists().map_err(|_| Error::Conflict)? {
            return Err(Error::Conflict);
        }
        let checkout = Checkout {
            workspace: context.workspace.clone(),
            repository: repository.id.clone(),
            operation: context.operation.clone(),
            name: context.workspace.to_string(),
            path: node_path(repository, &path)?,
            revision,
            base_commit: commit,
            base_change: change,
        };
        Ok(PreparedWorkspace {
            path: Some(checkout.path.clone()),
            intent: data("hive.jj.checkout", &checkout)?,
        })
    }
    fn at_location(
        &self,
        intent: &BackendData,
        path: &NodePath,
        _verify_creation: bool,
    ) -> Result<BackendData, Error> {
        let checkout: Checkout = intent.decode("hive.jj.checkout")?;
        // An externally supplied reconciliation path is evidence to verify, never a relocation request.
        if &checkout.path != path {
            return Err(Error::Conflict);
        }
        data("hive.jj.checkout", &checkout)
    }
    fn create(
        &self,
        repository: Option<&RepositoryHandle>,
        intent: &BackendData,
        checkpoints: &mut dyn CheckpointWriter<Error = Error>,
    ) -> Result<WorkspaceObservation, Error> {
        let repository = repository.ok_or(Error::Conflict)?;
        let checkout: Checkout = intent.decode("hive.jj.checkout")?;
        RepositoryBackend::verify(self, repository)?;
        let store = storage(repository)?;
        run(
            Path::new(store.root.as_str()),
            &[
                "workspace",
                "add",
                "--no-colocate",
                "--sparse-patterns",
                "full",
                "--name",
                &checkout.name,
                "--revision",
                &checkout.base_commit,
                "--",
                checkout.path.as_str(),
            ],
        )?;
        let registration = self.identity(repository, &checkout, false)?;
        let actual = Path::new(&registration.1)
            .canonicalize()
            .map_err(|_| Error::Conflict)?;
        checkpoints.checkpoint(data("hive.workspace.path", &actual)?)?;
        write_marker(
            &Path::new(checkout.path.as_str()).join(".jj/hive-workspace.json"),
            &checkout,
        )?;
        self.observe(repository, &checkout)
    }
    fn inspect(
        &self,
        repository: Option<&RepositoryHandle>,
        intent: &BackendData,
    ) -> Result<WorkspaceObservation, Error> {
        self.observe(
            repository.ok_or(Error::Conflict)?,
            &intent.decode("hive.jj.checkout")?,
        )
    }
    fn reconcile(
        &self,
        repository: Option<&RepositoryHandle>,
        intent: &BackendData,
    ) -> Result<WorkspaceObservation, Error> {
        let repository = repository.ok_or(Error::Conflict)?;
        let checkout: Checkout = intent.decode("hive.jj.checkout")?;
        self.identity(repository, &checkout, false)?;
        let root = Path::new(checkout.path.as_str());
        let marker = root.join(".jj/hive-workspace.json");
        if !marker.try_exists().map_err(|_| Error::Conflict)? {
            // Explicit reconciliation may finish ownership marking only for an unchanged new child.
            let parents = commits(root, "parents(@)")?;
            if parents.len() != 1 || parents[0].0 != checkout.base_commit {
                return Err(Error::Conflict);
            }
            write_marker(&marker, &checkout)?;
        }
        self.observe(repository, &checkout)
    }
    fn is_absent(
        &self,
        repository: Option<&RepositoryHandle>,
        intent: &BackendData,
    ) -> Result<bool, Error> {
        let repository = repository.ok_or(Error::Conflict)?;
        RepositoryBackend::verify(self, repository)?;
        let checkout: Checkout = intent.decode("hive.jj.checkout")?;
        let store = storage(repository)?;
        if !std::fs::symlink_metadata(checkout.path.as_str())
            .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
        {
            return Ok(false);
        }
        for registration in registrations(Path::new(store.root.as_str()))? {
            if registration.0 == checkout.name || same_path(&registration.1, &checkout.path)? {
                return Ok(false);
            }
        }
        Ok(true)
    }
    fn remove(
        &self,
        repository: Option<&RepositoryHandle>,
        intent: &BackendData,
        discard: bool,
    ) -> Result<(), Error> {
        // JJ removal requires a durable barrier; callers must use remove_checkpointed.
        let _ = (repository, intent, discard);
        Err(Error::Uncertain)
    }
    fn remove_checkpointed(
        &self,
        repository: Option<&RepositoryHandle>,
        intent: &BackendData,
        discard: bool,
        operation: &OperationId,
        checkpoints: &mut dyn CheckpointWriter<Error = Error>,
    ) -> Result<(), Error> {
        let repository = repository.ok_or(Error::Conflict)?;
        let checkout: Checkout = intent.decode("hive.jj.checkout")?;
        let observed = self.observe(repository, &checkout)?;
        if !discard && observed.cleanliness != Cleanliness::Clean {
            return Err(Error::Uncertain);
        }
        let store = storage(repository)?;
        let root = Path::new(store.root.as_str());
        let registration = self.identity(repository, &checkout, true)?;
        // A real bookmark prevents JJ from abandoning/GC'ing history after workspace forget.
        let bookmark = format!("hive-retained-{}", checkout.workspace);
        let existing = commits(root, &format!("bookmarks(exact:{bookmark})"))?;
        if !existing.is_empty() && (existing.len() != 1 || existing[0].0 != registration.2) {
            return Err(Error::Conflict);
        }
        if existing.is_empty() {
            run(
                root,
                &[
                    "bookmark",
                    "create",
                    &bookmark,
                    "--revision",
                    &registration.2,
                ],
            )?;
        }
        let removal = Removal {
            repository: repository.clone(),
            checkout: checkout.clone(),
            operation: operation.clone(),
            commit: registration.2,
            bookmark,
        };
        if !retained(&removal)? {
            return Err(Error::Uncertain);
        }
        checkpoints.checkpoint(data("hive.jj.removal", &removal)?)?;
        // Delete files first: an interrupted removal retains native registration and history evidence.
        std::fs::remove_dir_all(checkout.path.as_str()).map_err(|_| Error::Removal)?;
        run(root, &["workspace", "forget", &checkout.name])?;
        Ok(())
    }
    fn verify_preserved(
        &self,
        _archive: Option<&NodePath>,
        checkpoint: Option<&BackendData>,
        operation: &OperationId,
    ) -> Result<bool, Error> {
        let Some(checkpoint) = checkpoint else {
            return Ok(false);
        };
        let removal: Removal = checkpoint.decode("hive.jj.removal")?;
        Ok(&removal.operation == operation && retained(&removal)?)
    }
    fn finish_removal(
        &self,
        repository: Option<&RepositoryHandle>,
        intent: &BackendData,
        checkpoint: Option<&BackendData>,
        operation: &OperationId,
    ) -> Result<bool, Error> {
        let repository = repository.ok_or(Error::Conflict)?;
        let Some(checkpoint) = checkpoint else {
            return Ok(false);
        };
        let removal: Removal = checkpoint.decode("hive.jj.removal")?;
        let checkout: Checkout = intent.decode("hive.jj.checkout")?;
        if removal.repository != *repository
            || removal.checkout != checkout
            || &removal.operation != operation
            || !retained(&removal)?
        {
            return Err(Error::Conflict);
        }
        if !std::fs::symlink_metadata(checkout.path.as_str())
            .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
        {
            return Err(Error::Conflict);
        }
        let store = storage(repository)?;
        let root = Path::new(store.root.as_str());
        let registrations = registrations(root)?;
        if let Some(registration) = registrations.iter().find(|r| r.0 == checkout.name) {
            if !same_path(&registration.1, &checkout.path)? || registration.2 != removal.commit {
                return Err(Error::Conflict);
            }
            run(root, &["workspace", "forget", &checkout.name])?;
        }
        self.is_absent(Some(repository), intent)
    }
}

fn clean(root: &Path) -> Result<Cleanliness, Error> {
    let before = commits(root, "@")?;
    let [Commit(commit, _, empty, conflict)] = before.as_slice() else {
        return Ok(Cleanliness::Unknown);
    };
    if !empty || *conflict {
        return Ok(Cleanliness::Dirty);
    }
    let files: Vec<(String, String, bool, String)> = lines(&run(
        root,
        &[
            "--no-integrate-operation",
            "file",
            "list",
            "--revision",
            commit,
            "--template",
            FILES,
        ],
    )?)?;
    let mut expected = BTreeSet::new();
    for (name, kind, executable, object) in files {
        let relative = Path::new(&name);
        if relative.is_absolute()
            || relative
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return Err(Error::Uncertain);
        }
        let path = root.join(relative);
        let mut ancestor = path.parent();
        while let Some(parent) = ancestor.filter(|parent| *parent != root) {
            if std::fs::symlink_metadata(parent)
                .map_err(|_| Error::Uncertain)?
                .file_type()
                .is_symlink()
            {
                return Ok(Cleanliness::Dirty);
            }
            ancestor = parent.parent();
        }
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(_) => return Ok(Cleanliness::Dirty),
        };
        // Unsupported types are uncertainty, never permission to remove them.
        if kind != "file" && kind != "symlink" {
            return Ok(Cleanliness::Unknown);
        }
        if (kind == "symlink") != metadata.file_type().is_symlink()
            || (kind == "file" && !metadata.is_file())
        {
            return Ok(Cleanliness::Dirty);
        }
        #[cfg(unix)]
        if kind == "file" {
            use std::os::unix::fs::PermissionsExt;
            if (metadata.permissions().mode() & 0o111 != 0) != executable {
                return Ok(Cleanliness::Dirty);
            }
        }
        #[cfg(not(unix))]
        let _ = executable;
        // Hash stdout while draining: large/binary files cannot exhaust memory or be truncated.
        let expected_hash = if kind == "symlink" {
            // JJ file show omits symlink targets. Resolve its native TreeValue through
            // this backend's explicit Git store, without following the filesystem link.
            let git_root = run(root, &["--no-integrate-operation", "git", "root"])?;
            execute_output_mode(
                "git",
                Some(root),
                &["--git-dir", git_root.trim(), "cat-file", "blob", &object],
                false,
                None,
                true,
            )?
        } else {
            run_output(
                root,
                &[
                    "--no-integrate-operation",
                    "file",
                    "show",
                    "--revision",
                    commit,
                    "--template",
                    "\"\"",
                    "--",
                    &format!(
                        "root:{}",
                        serde_json::to_string(&name).map_err(|_| Error::Uncertain)?
                    ),
                ],
                true,
            )?
        };
        use sha2::{Digest, Sha256};
        let mut hash = Sha256::new();
        if kind == "symlink" {
            hash.update(
                std::fs::read_link(&path)
                    .map_err(|_| Error::Uncertain)?
                    .to_str()
                    .ok_or(Error::Uncertain)?
                    .as_bytes(),
            );
        } else {
            use std::io::Read;
            let mut file = std::fs::File::open(&path).map_err(|_| Error::Uncertain)?;
            let mut buffer = [0; 8192];
            loop {
                let count = file.read(&mut buffer).map_err(|_| Error::Uncertain)?;
                if count == 0 {
                    break;
                }
                hash.update(&buffer[..count]);
            }
        }
        if hash.finalize().to_vec() != expected_hash {
            return Ok(Cleanliness::Dirty);
        }
        expected.insert(relative.to_path_buf());
    }
    let mut pending = vec![root.to_path_buf()];
    let mut count = 0;
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(&directory).map_err(|_| Error::Uncertain)? {
            let entry = entry.map_err(|_| Error::Uncertain)?;
            count += 1;
            if count > 100_000 {
                return Ok(Cleanliness::Unknown);
            }
            let path = entry.path();
            if path == root.join(".jj") {
                continue;
            }
            let kind = entry.file_type().map_err(|_| Error::Uncertain)?;
            if kind.is_dir() {
                pending.push(path);
            } else if !expected.remove(path.strip_prefix(root).map_err(|_| Error::Uncertain)?) {
                return Ok(Cleanliness::Dirty);
            }
        }
    }
    let after = commits(root, "@")?;
    if after.len() != 1 || after[0].0 != *commit || !expected.is_empty() {
        return Ok(Cleanliness::Unknown);
    }
    Ok(Cleanliness::Clean)
}
