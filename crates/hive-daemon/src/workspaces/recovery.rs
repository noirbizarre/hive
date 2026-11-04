//! Recovery observes durable intent; only explicit sweep application executes removal.
use super::*;

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
pub(super) fn crash_boundary(phase: &str) {
    if std::env::var("HIVE_WORKSPACE_CRASH_PHASE").as_deref() == Ok(phase) {
        // A pipe handshake lets the parent kill the actual process at a durable boundary.
        println!("HIVE_WORKSPACE_CRASH_READY");
        std::io::Write::flush(&mut std::io::stdout()).unwrap();
        loop {
            std::thread::park();
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Transition {
    state: WorkspaceState,
    operation: Option<OperationId>,
    retry: Option<RetryDisposition>,
    at: u64,
}

#[derive(Serialize)]
pub struct RecoveryReport {
    node: NodeId,
    observed_at: u64,
    workspaces: Vec<WorkspaceReport>,
    repositories: Vec<Repository>,
    repository_findings: Vec<serde_json::Value>,
    orphans: Vec<Orphan>,
    scan_errors: Vec<serde_json::Value>,
}

#[derive(Serialize)]
struct WorkspaceReport {
    workspace: Entry,
    path: Option<PathBuf>,
    classifications: Vec<&'static str>,
    sessions: Vec<String>,
    inspection: Option<Inspection>,
    failure: Option<serde_json::Value>,
    retention_eligible: bool,
    last_used_at: Option<u64>,
    expires_at: Option<u64>,
    cleanup_blocker: Option<serde_json::Value>,
    transitions: Vec<Transition>,
    help: &'static str,
}

#[derive(Serialize)]
struct Orphan {
    path: PathBuf,
    reason: &'static str,
    help: &'static str,
}

#[derive(Debug, thiserror::Error)]
enum ScanFailure {
    #[error("refused to follow a symbolic link during resource discovery")]
    Symlink,
    #[error("could not list a resource directory ({0:?})")]
    Read(std::io::ErrorKind),
}

impl ScanFailure {
    fn json(&self, path: &Path) -> serde_json::Value {
        let (code, help) = match self {
            Self::Symlink => (
                "hive::workspace::scan_symlink",
                "inspect the linked directory manually; restore the expected node-local directory before retrying discovery",
            ),
            Self::Read(_) => (
                "hive::workspace::scan_failed",
                "check directory access, filesystem health and permissions on the owning node, then retry recovery inspection",
            ),
        };
        // Retain the controlled I/O category, never raw OS or subprocess diagnostics.
        serde_json::json!({"path":path,"code":code,"message":self.to_string(),"help":help})
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SweepRequest {
    pub operation: OperationId,
    pub workspaces: Option<Vec<WorkspaceId>>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Sweep {
    request: SweepRequest,
    created_at: u64,
    items: Vec<SweepItem>,
}

#[derive(Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SweepStatus {
    Planned,
    Removed,
    AlreadyRemoved,
    Blocked,
    RetryableFailure,
    ManualIntervention,
}

#[derive(Clone, Serialize, Deserialize)]
struct SweepItem {
    workspace: WorkspaceId,
    operation: OperationId,
    // Freeze the entire backend request: later reconciliation may change observed locations.
    cleanup: Option<CleanupRequest>,
    status: SweepStatus,
    failure: Option<serde_json::Value>,
}

fn blocker(kind: &str, help: &str) -> serde_json::Value {
    serde_json::json!({"code":format!("hive::workspace::{kind}"), "message":kind.replace('_', " "), "help":help})
}

impl crate::Broker {
    pub(crate) fn recovery_projects(&self) -> Result<BTreeSet<ProjectId>, Error> {
        let registry = self.registry.lock().map_err(|_| Error::Storage)?;
        Ok(registry
            .current
            .projects
            .keys()
            .chain(registry.managed_projects.keys())
            .cloned()
            .collect())
    }

    pub(crate) async fn recover_workspaces(&self) -> Result<(), Error> {
        let broker = self.clone();
        self.workspace_service()?
            .run(move |rt| {
                rt.reclaim_unstarted(&broker)?;
                rt.recover_resources()
            })
            .await
    }
}

impl Service {
    pub async fn recovery(&self, projects: BTreeSet<ProjectId>) -> Result<RecoveryReport, Error> {
        self.run(move |rt| rt.report(&projects)).await
    }

    pub async fn plan_sweep(&self, request: SweepRequest) -> Result<Sweep, Error> {
        self.run(move |rt| rt.plan_sweep(request)).await
    }

    pub async fn sweep(&self, operation: OperationId) -> Result<Sweep, Error> {
        self.run(move |rt| {
            rt.state
                .sweeps
                .get(operation.as_str())
                .cloned()
                .ok_or(Error::Unknown)
        })
        .await
    }

    pub async fn apply_sweep(&self, operation: OperationId) -> Result<Sweep, Error> {
        let sweep = self.sweep(operation.clone()).await?;
        // Release admission between items so a large batch cannot starve attachment.
        for index in 0..sweep.items.len() {
            let operation = operation.clone();
            self.run(move |rt| rt.apply_item(&operation, index)).await?;
        }
        self.sweep(operation).await
    }
}

impl Runtime<'_> {
    pub(super) fn touch(&mut self, id: &WorkspaceId) {
        let used = self.state.last_used.entry(id.to_string()).or_default();
        // A clock correction must not shorten the observed retention window.
        *used = (*used).max(now());
    }

    pub(super) fn record_transitions(&mut self) {
        let at = now();
        for (id, entry) in &self.state.workspaces {
            let history = self.state.lifecycle.entry(id.clone()).or_default();
            let state = entry.workspace.state();
            let operation = entry.workspace.operation().cloned();
            let retry = entry.workspace.retry();
            if history.last().is_none_or(|last| {
                last.state != state || last.operation != operation || last.retry != retry
            }) {
                let at = history.last().map_or(at, |last| last.at.max(at));
                history.push(Transition {
                    state,
                    operation,
                    retry,
                    at,
                });
                if state == WorkspaceState::Ready {
                    let used = self.state.last_used.entry(id.clone()).or_default();
                    *used = (*used).max(at);
                }
            }
            // Old snapshots have no reliable activity timestamp. Give them a full idle window once.
            self.state.last_used.entry(id.clone()).or_insert(at);
        }
    }

    fn expiry(&self, entry: &Entry) -> Option<u64> {
        match entry.workspace.intent().retention {
            Retention::Retain => None,
            Retention::Until { expires_at } => Some(expires_at),
            Retention::Idle {
                last_used_at,
                idle_seconds,
            } => self
                .state
                .last_used
                .get(entry.workspace.id().as_str())
                .copied()
                .map(|used| used.max(last_used_at))
                .and_then(|used| used.checked_add(idle_seconds)),
        }
    }

    fn eligible(&self, entry: &Entry) -> bool {
        matches!(
            entry.workspace.intent().policy,
            WorkspacePolicy::Ephemeral { .. }
        ) && self.expiry(entry).is_some_and(|expiry| now() >= expiry)
    }

    fn uncertain_hooks(&self, entry: &Entry) -> bool {
        entry
            .workspace
            .backend()
            .is_some_and(|backend| backend.0 == "worktrunk")
            && matches!(
                entry.workspace.state(),
                WorkspaceState::Provisioning
                    | WorkspaceState::Failed
                    | WorkspaceState::Cleaning
                    | WorkspaceState::CleanupFailed
            )
            && entry
                .workspace
                .operation()
                .and_then(|operation| self.state.operations.get(operation.as_str()))
                .is_some_and(|operation| operation.effects_started)
    }

    fn cleanup_request(
        &self,
        entry: &Entry,
        operation: OperationId,
    ) -> Result<CleanupRequest, Error> {
        Ok(CleanupRequest {
            operation,
            target: ResourceTarget {
                workspace: entry.workspace.id().clone(),
                node: self.state.node.clone(),
                backend: entry.workspace.backend().cloned().ok_or(Error::Conflict)?,
                ownership: entry.workspace.intent().ownership(),
                location: entry.workspace.location().cloned(),
            },
            mode: CleanupMode::PreserveChanges,
        })
    }

    fn sweep_safety(
        &self,
        entry: &Entry,
        request: &CleanupRequest,
    ) -> Result<(), serde_json::Value> {
        if !self.eligible(entry) {
            return Err(blocker(
                "retained",
                "only expired ephemeral workspaces qualify; use explicit remove for persistent resources",
            ));
        }
        if matches!(
            entry.workspace.state(),
            WorkspaceState::Cleaning | WorkspaceState::CleanupFailed
        ) && entry.workspace.operation() != Some(&request.operation)
        {
            let mut failure = blocker(
                "cleanup_in_progress",
                "reconcile and retry the original cleanup operation or its original sweep; a new sweep cannot replace its durable intent",
            );
            failure["operation"] = serde_json::json!(entry.workspace.operation());
            return Err(failure);
        }
        if !matches!(
            entry.workspace.state(),
            WorkspaceState::Ready | WorkspaceState::Cleaning | WorkspaceState::CleanupFailed
        ) {
            return Err(blocker(
                "incomplete",
                "inspect and reconcile the original provisioning operation before cleanup",
            ));
        }
        request
            .target
            .check_node(&self.state.node)
            .map_err(|error| Error::Domain(error).json())?;
        if entry.workspace.backend() != Some(&request.target.backend)
            || entry.workspace.intent().ownership() != request.target.ownership
        {
            return Err(Error::Conflict.json());
        }
        let inspection = self
            .inspect(entry.workspace.id())
            .map_err(|error| error.json())?;
        let Inspection::Present { safety, .. } = inspection else {
            return Err(blocker(
                "unverified",
                "inspect and reconcile missing, partial or conflicting resources before cleanup",
            ));
        };
        request
            .check_safety(&safety)
            .map_err(|error| Error::Domain(error).json())
    }

    pub(super) fn recover_resources(&mut self) -> Result<(), Error> {
        // Persist legacy baselines once, before any retention observation.
        self.save()?;
        let entries: Vec<_> = self.state.workspaces.values().cloned().collect();
        for mut entry in entries {
            if entry
                .workspace
                .backend()
                .is_some_and(|backend| backend.0 == "worktrunk")
                || self.has_use(entry.workspace.id())
            {
                continue;
            }
            match entry.workspace.state() {
                WorkspaceState::Provisioning | WorkspaceState::Failed => {
                    // Inspection requires the ownership marker; unlike manual reconcile, this never claims a checkout.
                    match self.completed_creation(&entry) {
                        Ok(Some(location)) => {
                            entry
                                .workspace
                                .transition(WorkspaceEvent::ReconciledReady { location })?;
                            entry.failure = None;
                        }
                        Err(Error::Storage) => return Err(Error::Storage),
                        _ => {}
                    }
                }
                WorkspaceState::Cleaning | WorkspaceState::CleanupFailed
                    if self.verified_removal_absence(&entry).unwrap_or(false) =>
                {
                    let operation = entry
                        .workspace
                        .operation()
                        .cloned()
                        .ok_or(Error::Conflict)?;
                    self.finish_cleanup(&mut entry, &operation)?;
                }
                _ => {}
            }
            self.state
                .workspaces
                .insert(entry.workspace.id().to_string(), entry);
        }
        self.save()
    }

    fn completed_creation(&self, entry: &Entry) -> Result<Option<WorkspaceLocation>, Error> {
        let operation = self
            .state
            .operations
            .get(entry.workspace.operation().ok_or(Error::Conflict)?.as_str())
            .ok_or(Error::Conflict)?;
        if !operation.effects_started || operation.request.get("project").is_none() {
            return Ok(None);
        }
        let Inspection::Present { location, .. } = self.inspect(entry.workspace.id())? else {
            return Ok(None);
        };
        let path = Path::new(location.root.as_str());
        // A marker alone cannot settle interrupted provisioning: prove the original base and registration.
        // These creation checks do not constrain later user edits/rebases in an already-ready workspace.
        let location = self.location(
            entry,
            path,
            Some(operation.commit.as_ref().ok_or(Error::Conflict)?),
        )?;
        let repository = self
            .state
            .repositories
            .get(entry.repository.as_ref().ok_or(Error::Conflict)?.as_str())
            .ok_or(Error::Conflict)?;
        if location.backend.0 == "clone" {
            if git(path, &["config", "--get", "remote.origin.url"], "inspect")?.trim()
                != repository.source
            {
                return Err(Error::Conflict);
            }
        } else {
            let registrations = git(
                Path::new(repository.repository.root.as_str()),
                &["worktree", "list", "--porcelain", "-z"],
                "inspect",
            )?;
            if !registered_worktree(&registrations, path)? {
                return Err(Error::Conflict);
            }
        }
        Ok(Some(location))
    }

    pub(super) fn verified_archive(&self, entry: &Entry, operation: &OperationId) -> bool {
        let Some(plan) = self.state.archives.get(operation.as_str()) else {
            return false;
        };
        let path = &plan.destination;
        entry
            .archive
            .as_ref()
            .is_some_and(|archive| Path::new(archive.as_str()) == path)
            && path
                .symlink_metadata()
                .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
            && git(
                path,
                &["config", "--get", "hive.archiveOperation"],
                "archive",
            )
            .is_ok_and(|value| value.trim() == operation.as_str())
            && git(path, &["rev-parse", "HEAD"], "archive").is_ok_and(|value| value == plan.head)
            && git(
                path,
                &["for-each-ref", "--format=%(objectname) %(refname)"],
                "archive",
            )
            .is_ok_and(|value| value == plan.refs)
            && git(path, &["fsck", "--no-reflogs"], "archive").is_ok()
    }

    fn report(&self, projects: &BTreeSet<ProjectId>) -> Result<RecoveryReport, Error> {
        let mut workspaces = Vec::new();
        for entry in self.state.workspaces.values() {
            let id = entry.workspace.id();
            let operation = self
                .state
                .operations
                .values()
                .find(|op| &op.workspace == id && op.request.get("project").is_some());
            let path = entry
                .workspace
                .location()
                .map(|location| PathBuf::from(location.root.as_str()))
                .or_else(|| operation.and_then(|op| op.path.clone()));
            let sessions = self
                .state
                .uses
                .keys()
                .filter(|session| self.holds(session, id))
                .cloned()
                .collect::<Vec<_>>();
            let mut classifications = Vec::new();
            match entry.workspace.state() {
                WorkspaceState::Requested
                | WorkspaceState::Provisioning
                | WorkspaceState::Failed => classifications.push("incomplete_creation"),
                WorkspaceState::Cleaning | WorkspaceState::CleanupFailed => {
                    classifications.push("incomplete_cleanup")
                }
                WorkspaceState::Removed => classifications.push("removed"),
                WorkspaceState::Ready => {}
            }
            if !projects.contains(&entry.workspace.intent().project) {
                classifications.push("project_missing");
            }
            if sessions.is_empty() && entry.workspace.state() != WorkspaceState::Removed {
                classifications.push("unreferenced");
            }
            let (inspection, failure) = match self.inspect(id) {
                Ok(inspection) => (Some(inspection), None),
                Err(error) => (None, Some(error.json())),
            };
            if path.as_ref().is_some_and(|path| {
                path.symlink_metadata()
                    .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
            }) && entry.workspace.state() != WorkspaceState::Removed
            {
                classifications.push("missing_path");
            }
            match &inspection {
                Some(Inspection::IdentityConflict) => classifications.push("identity_conflict"),
                Some(Inspection::Partial { .. }) => classifications.push("partial"),
                _ => {}
            }
            let eligible = self.eligible(entry);
            if eligible {
                classifications.push("retention_expired");
            }
            if self.uncertain_hooks(entry) {
                classifications.push("uncertain_hooks");
            }
            let cleanup_blocker = if self.uncertain_hooks(entry) {
                Some(Error::Uncertain.json())
            } else {
                self.cleanup_request(entry, OperationId::new())
                    .map_err(|error| error.json())
                    .and_then(|request| self.sweep_safety(entry, &request))
                    .err()
            };
            workspaces.push(WorkspaceReport {
                workspace: entry.clone(), path, classifications, sessions, inspection, failure,
                retention_eligible: eligible,
                last_used_at: self.state.last_used.get(id.as_str()).copied(),
                expires_at: self.expiry(entry), cleanup_blocker,
                transitions: self.state.lifecycle.get(id.as_str()).cloned().unwrap_or_default(),
                help: "inspect the original operation and session holds; reconcile interrupted effects before explicit sweep/remove; unreferenced does not authorize deletion",
            });
        }
        let (orphans, scan_errors) = self.discover_orphans();
        let repository_findings = self.state.repositories.values().map(|repository| {
            let root = Path::new(repository.repository.root.as_str());
            let staging = root.parent().map(|parent| parent.join(".git-incomplete"));
            let failure = self.verify_repository(repository).err().map(|error| error.json());
            serde_json::json!({
                "repository":repository.repository.id,
                "incomplete":repository.status != "ready" || failure.is_some(),
                "staging":staging,
                "staging_present":staging.as_ref().is_some_and(|path| path.symlink_metadata().is_ok()),
                "failure":failure,
                "help":"retry the original create operation or explicit repository fetch; recovery never fetches, renames or deletes staging resources"
            })
        }).collect();
        Ok(RecoveryReport {
            node: self.state.node.clone(),
            observed_at: now(),
            workspaces,
            repositories: self.state.repositories.values().cloned().collect(),
            repository_findings,
            orphans,
            scan_errors,
        })
    }

    fn discover_orphans(&self) -> (Vec<Orphan>, Vec<serde_json::Value>) {
        let mut known = BTreeSet::new();
        for entry in self.state.workspaces.values() {
            if let Some(location) = entry.workspace.location() {
                known.insert(PathBuf::from(location.root.as_str()));
            }
        }
        known.extend(
            self.state
                .operations
                .values()
                .filter_map(|op| op.path.clone()),
        );
        for repository in self.state.repositories.values() {
            if let Some(parent) = Path::new(repository.repository.root.as_str()).parent() {
                known.insert(parent.join(".git-incomplete"));
            }
        }
        let mut roots = vec![self.store.state_directory().join("clones")];
        roots.extend(self.state.repositories.values().filter_map(|repo| {
            Path::new(repo.repository.root.as_str())
                .parent()
                .map(Path::to_path_buf)
        }));
        let mut orphans = Vec::new();
        let mut errors = Vec::new();
        let mut remaining = 4096usize;
        // Scan only fixed layout levels, never recurse into checkout contents or symlinks.
        roots.push(self.root());
        for root in roots {
            if root
                .symlink_metadata()
                .is_ok_and(|metadata| metadata.file_type().is_symlink())
            {
                errors.push(ScanFailure::Symlink.json(&root));
                continue;
            }
            let entries = match std::fs::read_dir(&root) {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    errors.push(ScanFailure::Read(error.kind()).json(&root));
                    continue;
                }
            };
            for item in entries {
                if remaining == 0 {
                    errors.push(blocker("scan_truncated", "inspect remaining resources manually; discovery is bounded to 4096 entries"));
                    return (orphans, errors);
                }
                remaining -= 1;
                let item = match item {
                    Ok(item) => item,
                    Err(error) => {
                        errors.push(ScanFailure::Read(error.kind()).json(&root));
                        continue;
                    }
                };
                let path = item.path();
                if known.contains(&path)
                    || self.state.repositories.values().any(|repo| {
                        Path::new(repo.repository.root.as_str()).parent() == Some(path.as_path())
                            || Path::new(repo.repository.root.as_str()) == path
                    })
                {
                    continue;
                }
                orphans.push(Orphan { path, reason: "unrecorded_resource", help: "inspect manually; an unrecorded path is never automatically adopted or deleted" });
            }
        }
        for repo in self.state.repositories.values() {
            if self.verify_repository(repo).is_err() {
                continue;
            }
            match git(
                Path::new(repo.repository.root.as_str()),
                &["worktree", "list", "--porcelain", "-z"],
                "inspect",
            ) {
                Ok(list) => {
                    for field in list.split('\0') {
                        if let Some(path) = field.strip_prefix("worktree ") {
                            let path = PathBuf::from(path);
                            if !known.contains(&path)
                                && path != Path::new(repo.repository.root.as_str())
                            {
                                if remaining == 0 {
                                    errors.push(blocker(
                                        "scan_truncated",
                                        "inspect remaining registered worktrees manually",
                                    ));
                                    break;
                                }
                                remaining -= 1;
                                orphans.push(Orphan { path, reason: "unrecorded_registration", help: "inspect this known repository's worktree registration manually; Hive has no deletion authority" });
                            }
                        }
                    }
                }
                Err(error) => errors.push(error.json()),
            }
        }
        (orphans, errors)
    }

    fn plan_sweep(&mut self, mut request: SweepRequest) -> Result<Sweep, Error> {
        if let Some(ids) = &mut request.workspaces {
            ids.sort();
            ids.dedup();
        }
        if let Some(sweep) = self.state.sweeps.get(request.operation.as_str()) {
            return if sweep.request == request {
                Ok(sweep.clone())
            } else {
                Err(Error::RetryConflict)
            };
        }
        if self.operation_reserved(&request.operation) {
            return Err(Error::RetryConflict);
        }
        self.save()?;
        let ids = request.workspaces.clone().unwrap_or_else(|| {
            self.state
                .workspaces
                .values()
                .filter(|entry| {
                    self.eligible(entry) && entry.workspace.state() != WorkspaceState::Removed
                })
                .map(|entry| entry.workspace.id().clone())
                .collect()
        });
        let mut sweep = Sweep {
            request,
            created_at: now(),
            items: Vec::new(),
        };
        for workspace in ids {
            let mut operation = OperationId::new();
            while self.operation_reserved(&operation)
                || operation == sweep.request.operation
                || sweep.items.iter().any(|item| item.operation == operation)
            {
                operation = OperationId::new();
            }
            let entry = self.state.workspaces.get(workspace.as_str());
            let cleanup = entry
                .map(|entry| self.cleanup_request(entry, operation.clone()))
                .transpose();
            let (cleanup, failure) = match cleanup {
                Ok(Some(cleanup)) => {
                    let failure = self.sweep_safety(entry.unwrap(), &cleanup).err();
                    (Some(cleanup), failure)
                }
                Ok(None) => (None, Some(Error::Unknown.json())),
                Err(error) => (None, Some(error.json())),
            };
            sweep.items.push(SweepItem {
                workspace,
                operation,
                cleanup,
                status: if failure.is_some() {
                    SweepStatus::Blocked
                } else {
                    SweepStatus::Planned
                },
                failure,
            });
        }
        self.state
            .sweeps
            .insert(sweep.request.operation.to_string(), sweep.clone());
        self.save()?;
        #[cfg(test)]
        crash_boundary("planned");
        Ok(sweep)
    }

    pub(super) fn operation_reserved(&self, operation: &OperationId) -> bool {
        self.state.operations.contains_key(operation.as_str())
            || self.state.sweeps.contains_key(operation.as_str())
            || self
                .state
                .sweeps
                .values()
                .any(|sweep| sweep.items.iter().any(|item| &item.operation == operation))
    }

    pub(super) fn planned_cleanup(&self, cleanup: &CleanupRequest) -> bool {
        self.state
            .sweeps
            .values()
            .flat_map(|sweep| &sweep.items)
            .any(|item| item.cleanup.as_ref() == Some(cleanup))
    }

    fn apply_item(&mut self, operation: &OperationId, index: usize) -> Result<(), Error> {
        let mut item = self
            .state
            .sweeps
            .get(operation.as_str())
            .and_then(|sweep| sweep.items.get(index))
            .cloned()
            .ok_or(Error::Unknown)?;
        let result = (|| {
            let request = item.cleanup.clone().ok_or(Error::Conflict)?;
            let entry = self
                .state
                .workspaces
                .get(item.workspace.as_str())
                .cloned()
                .ok_or(Error::Unknown)?;
            if entry.workspace.state() == WorkspaceState::Removed {
                if self.verified_removal_absence(&entry)? {
                    return Ok(SweepStatus::AlreadyRemoved);
                }
                return Err(Error::Conflict);
            }
            if self.uncertain_hooks(&entry) {
                return Err(Error::Uncertain);
            }
            // A resource may be complete even when the crash prevented batch-result publication.
            if entry.workspace.operation() == Some(&item.operation)
                && matches!(
                    entry.workspace.state(),
                    WorkspaceState::Cleaning | WorkspaceState::CleanupFailed
                )
                && !entry
                    .workspace
                    .backend()
                    .is_some_and(|backend| backend.0 == "worktrunk")
                && self.verified_removal_absence(&entry)?
            {
                let mut entry = entry;
                self.finish_cleanup(&mut entry, &item.operation)?;
                self.state
                    .workspaces
                    .insert(item.workspace.to_string(), entry);
                self.save()?;
                return Ok(SweepStatus::Removed);
            }
            if let Err(failure) = self.sweep_safety(&entry, &request) {
                item.failure = Some(failure);
                return Ok(SweepStatus::Blocked);
            }
            self.remove(
                item.workspace.clone(),
                item.operation.clone(),
                false,
                Some(request),
            )?;
            #[cfg(test)]
            crash_boundary("removed");
            Ok(SweepStatus::Removed)
        })();
        match result {
            Ok(status) => {
                item.status = status;
                if status != SweepStatus::Blocked {
                    item.failure = None;
                }
            }
            Err(error) => {
                // Durable-state failure must abort the batch, not masquerade as an item refusal.
                if matches!(error, Error::Storage) {
                    return Err(error);
                }
                item.status = if matches!(
                    error,
                    Error::Uncertain | Error::Conflict | Error::RetryConflict
                ) {
                    SweepStatus::ManualIntervention
                } else {
                    SweepStatus::RetryableFailure
                };
                item.failure = Some(error.json());
            }
        }
        self.state
            .sweeps
            .get_mut(operation.as_str())
            .ok_or(Error::Unknown)?
            .items[index] = item;
        self.save()
    }
}
