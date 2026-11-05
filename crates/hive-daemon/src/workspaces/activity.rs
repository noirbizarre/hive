//! WorkTrunk marker effects are serialized with resource admission, never session locks.

use super::*;
use crate::worktrunk::{Status, aggregate};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Snapshot {
    pub session: String,
    pub cwd: PathBuf,
    pub workspace: Option<String>,
    pub status: Status,
    pub order: u64,
    pub restored_idle: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct Target {
    node: NodeId,
    common: PathBuf,
    branch: String,
    // A replacement repository at the same path must not inherit old marker cleanup authority.
    identity: Option<(u64, u64)>,
}

fn directory_identity(path: &Path) -> Result<Option<(u64, u64)>, Error> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = path.metadata().map_err(|_| Error::Conflict)?;
        Ok(Some((metadata.dev(), metadata.ino())))
    }
    #[cfg(not(unix))]
    {
        // Windows exposes creation time even though portable metadata has no inode identity.
        let metadata = path.metadata().map_err(|_| Error::Conflict)?;
        let created = metadata
            .created()
            .map_err(|_| Error::Conflict)?
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| Error::Conflict)?;
        Ok(Some((created.as_secs(), u64::from(created.subsec_nanos()))))
    }
}

#[derive(Serialize, Deserialize)]
struct Marker {
    target: Target,
    confirmed: serde_json::Value,
    // A unique intended value identifies a write even if Hive dies before reading its timestamp.
    pending: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct Ledger {
    version: u32,
    markers: BTreeMap<String, Marker>,
    #[serde(default)]
    contributors: BTreeMap<String, (Status, u64)>,
    #[serde(default)]
    sequence: u64,
    #[serde(default)]
    observed: BTreeMap<String, u64>,
}

impl Default for Ledger {
    fn default() -> Self {
        Self {
            version: 1,
            markers: BTreeMap::new(),
            contributors: BTreeMap::new(),
            sequence: 0,
            observed: BTreeMap::new(),
        }
    }
}

impl Service {
    pub(crate) async fn check_activity(
        &self,
        cwd: PathBuf,
        workspace: Option<String>,
        config: WorktrunkConfig,
    ) -> Result<(), Error> {
        if config.enabled != Some(true) {
            return Ok(());
        }
        self.run(move |rt| {
            let snapshot = Snapshot {
                session: String::new(),
                cwd,
                workspace,
                status: Status::Unknown,
                order: 0,
                restored_idle: false,
            };
            if rt.activity_target(&snapshot)?.is_some() {
                config.select(probe_activity).map_err(Error::Domain)?;
            }
            Ok(())
        })
        .await
    }

    pub(crate) async fn publish_activity(
        &self,
        snapshots: Vec<Snapshot>,
        config: WorktrunkConfig,
    ) -> Result<(), Error> {
        // Disabled means no wt invocation, including deferred cleanup of previously owned markers.
        if config.enabled == Some(false) {
            return Ok(());
        }
        self.run(move |rt| rt.publish_activity(snapshots, config))
            .await
    }
}

fn short_git(path: &Path, args: &[&str]) -> Result<String, Error> {
    execute_with_timeout("git", Some(path), args, false, None, Duration::from_secs(3))
}

fn marker_command(path: Option<&Path>, args: &[&str]) -> Result<String, Error> {
    execute_with_timeout("wt", path, args, true, None, Duration::from_secs(3))
}

pub(super) fn probe_activity() -> WorktrunkAvailability {
    let Ok(help) = marker_command(None, &["config", "state", "marker", "--help"]) else {
        return WorktrunkAvailability::Unavailable;
    };
    if !["get", "set", "clear", "--format"]
        .iter()
        .all(|flag| help.contains(flag))
    {
        return WorktrunkAvailability::Incompatible;
    }
    for command in ["get", "set", "clear"] {
        if !marker_command(None, &["config", "state", "marker", command, "--help"])
            .is_ok_and(|help| help.contains("--branch"))
        {
            return WorktrunkAvailability::Incompatible;
        }
    }
    WorktrunkAvailability::Usable
}

fn read_marker(target: &Target) -> Result<serde_json::Value, Error> {
    let output = marker_command(
        Some(&target.common),
        &[
            "config",
            "state",
            "marker",
            "--format=json",
            "get",
            &format!("--branch={}", target.branch),
        ],
    )?;
    let value: serde_json::Value = serde_json::from_str(&output).map_err(|_| Error::Worktrunk)?;
    if value.is_null()
        || (value["branch"] == target.branch
            && value["marker"].is_string()
            && value["set_at"].is_u64())
    {
        Ok(value)
    } else {
        Err(Error::Worktrunk)
    }
}

impl Runtime<'_> {
    fn activity_target(&self, snapshot: &Snapshot) -> Result<Option<Target>, Error> {
        if let Some(id) = &snapshot.workspace {
            let entry = self.state.workspaces.get(id).ok_or(Error::Unknown)?;
            entry
                .workspace
                .intent()
                .check_node(&self.state.node)
                .map_err(Error::Domain)?;
            let selection = self.selection(entry)?;
            if selection
                .repository
                .as_ref()
                .is_some_and(|repository| repository.backend.0 != "git")
            {
                return Ok(None);
            }
            // Validate pinned implementations before looking at paths or probing optional executables.
            self.manager.resolve(&selection)?;
            if let Some(repository) = &selection.repository {
                self.manager.repository(repository)?.verify(repository)?;
            }
        }
        if !snapshot.cwd.is_dir() {
            return Ok(None);
        }
        let Ok(root) = short_git(&snapshot.cwd, &["rev-parse", "--show-toplevel"]) else {
            return Ok(None);
        };
        let root = PathBuf::from(root.trim())
            .canonicalize()
            .map_err(|_| Error::Conflict)?;
        if !snapshot
            .cwd
            .canonicalize()
            .map_err(|_| Error::Conflict)?
            .starts_with(&root)
        {
            return Err(Error::Conflict);
        }
        if let Some(id) = &snapshot.workspace {
            let entry = &self.state.workspaces[id];
            if entry.workspace.intent().ownership() != Ownership::External {
                let location = entry.workspace.location().ok_or(Error::Conflict)?;
                if Path::new(location.root.as_str())
                    .canonicalize()
                    .map_err(|_| Error::Conflict)?
                    != root
                    || short_git(&root, &["config", "--get", "hive.workspace"])?.trim() != id
                {
                    return Err(Error::Conflict);
                }
            }
        }
        let common = PathBuf::from(
            short_git(
                &root,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            )?
            .trim(),
        )
        .canonicalize()
        .map_err(|_| Error::Conflict)?;
        let Ok(branch) = short_git(&root, &["symbolic-ref", "--quiet", "HEAD"]) else {
            return Ok(None);
        };
        let Some(branch) = branch.trim().strip_prefix("refs/heads/") else {
            return Ok(None);
        };
        let list = short_git(&common, &["worktree", "list", "--porcelain", "-z"])?;
        if !registered_worktree(&list, &root)? {
            return Err(Error::Conflict);
        }
        let identity = directory_identity(&common)?;
        Ok(Some(Target {
            node: self.state.node.clone(),
            common,
            branch: branch.to_owned(),
            identity,
        }))
    }

    fn publish_activity(
        &mut self,
        mut snapshots: Vec<Snapshot>,
        config: WorktrunkConfig,
    ) -> Result<(), Error> {
        let mut ledger = self
            .store
            .resource::<Ledger>("worktrunk.activity")?
            .unwrap_or_default();
        if ledger.version != 1 {
            return Err(Error::Conflict);
        }
        let mut desired: BTreeMap<String, (Target, Vec<(Status, u64)>)> = BTreeMap::new();
        let mut failure = None;
        // HashMap session traversal order cannot decide which simultaneous terminal event is latest.
        snapshots.sort_by_key(|snapshot| snapshot.order);
        for snapshot in snapshots {
            let target = match self.activity_target(&snapshot) {
                Ok(target) => target,
                Err(error) => {
                    failure = Some(error);
                    continue;
                }
            };
            if let Some(target) = target {
                let previous = ledger.contributors.get(&snapshot.session).copied();
                let current = if snapshot.restored_idle && snapshot.status == Status::Unknown {
                    // Only current recovered idle can retain an outcome; replay itself never creates one.
                    previous
                        .filter(|(s, _)| {
                            matches!(s, Status::Completed | Status::Cancelled | Status::Failed)
                        })
                        .unwrap_or((Status::Unknown, 0))
                } else if previous.is_some_and(|(status, _)| status == snapshot.status)
                    && ledger.observed.get(&snapshot.session) == Some(&snapshot.order)
                {
                    previous.expect("existing contributor")
                } else {
                    ledger.sequence = ledger.sequence.saturating_add(1);
                    (snapshot.status, ledger.sequence)
                };
                ledger
                    .observed
                    .insert(snapshot.session.clone(), snapshot.order);
                ledger.contributors.insert(snapshot.session, current);
                let key = serde_json::to_string(&target).map_err(|_| Error::Storage)?;
                desired
                    .entry(key)
                    .or_insert_with(|| (target, vec![]))
                    .1
                    .push(current);
            }
        }
        let retained: BTreeSet<_> = self
            .store
            .records()?
            .into_iter()
            .map(|record| record.id)
            .collect();
        ledger.contributors.retain(|id, _| retained.contains(id));
        ledger.observed.retain(|id, _| retained.contains(id));
        if desired.is_empty() && ledger.markers.is_empty() {
            return failure.map_or(Ok(()), Err);
        }
        if config.select(probe_activity).map_err(Error::Domain)? != IntegrationSelection::Worktrunk
        {
            return Ok(());
        }
        for (key, (target, _)) in &desired {
            ledger.markers.entry(key.clone()).or_insert_with(|| Marker {
                target: target.clone(),
                confirmed: serde_json::Value::Null,
                pending: None,
            });
        }
        // Save target bindings before any effect: orphaned session rows are not needed for cleanup.
        self.store.save_resource("worktrunk.activity", &ledger)?;
        let keys: Vec<_> = ledger.markers.keys().cloned().collect();
        for key in keys {
            // An inaccessible repository must not prevent healthy repositories from reconciling.
            let result = (|| -> Result<(), Error> {
                let marker = ledger.markers.get(&key).expect("recorded marker");
                if marker.target.node != self.state.node {
                    return Err(Error::Conflict);
                }
                let common = PathBuf::from(
                    short_git(
                        &marker.target.common,
                        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
                    )?
                    .trim(),
                )
                .canonicalize()
                .map_err(|_| Error::Conflict)?;
                if common != marker.target.common
                    || directory_identity(&common)? != marker.target.identity
                {
                    return Err(Error::Conflict);
                }
                let actual = read_marker(&marker.target)?;
                let pending_matches = marker
                    .pending
                    .as_ref()
                    .is_some_and(|pending| actual["marker"].as_str() == Some(pending));
                if !actual.is_null() && actual != marker.confirmed && !pending_matches {
                    // WorkTrunk has one shared marker: a foreign value is never ours to replace/clear.
                    tracing::warn!(
                        diagnostic_code = "hive::workspace::activity_conflict",
                        "WorkTrunk marker changed externally; preserving it"
                    );
                    ledger.markers.remove(&key);
                    self.store.save_resource("worktrunk.activity", &ledger)?;
                    return Ok(());
                }
                let status = desired.get(&key).map_or(Status::Unknown, |(_, values)| {
                    aggregate(values.iter().copied())
                });
                let label = status.marker();
                if label.is_some_and(|label| {
                    actual["marker"]
                        .as_str()
                        .is_some_and(|value| value.starts_with(&format!("{label} [")))
                }) {
                    let marker = ledger.markers.get_mut(&key).expect("recorded marker");
                    marker.confirmed = actual;
                    marker.pending = None;
                    self.store.save_resource("worktrunk.activity", &ledger)?;
                    return Ok(());
                }
                let target = ledger.markers[&key].target.clone();
                if let Some(label) = label {
                    // The token survives a lost set response; identical emoji from another agent cannot claim ownership.
                    let value = format!("{label} [{}]", uuid::Uuid::new_v4());
                    ledger
                        .markers
                        .get_mut(&key)
                        .expect("recorded marker")
                        .pending = Some(value.clone());
                    self.store.save_resource("worktrunk.activity", &ledger)?;
                    marker_command(
                        Some(&target.common),
                        &[
                            "config",
                            "state",
                            "marker",
                            "set",
                            &value,
                            &format!("--branch={}", target.branch),
                        ],
                    )?;
                    let actual = read_marker(&target)?;
                    if actual["marker"].as_str() != Some(&value) {
                        return Err(Error::Conflict);
                    }
                    let marker = ledger.markers.get_mut(&key).expect("recorded marker");
                    marker.confirmed = actual;
                    marker.pending = None;
                } else {
                    // The original branch is explicit even if the session's directory has changed branches.
                    if !actual.is_null() {
                        marker_command(
                            Some(&target.common),
                            &[
                                "config",
                                "state",
                                "marker",
                                "clear",
                                &format!("--branch={}", target.branch),
                            ],
                        )?;
                    }
                    ledger.markers.remove(&key);
                }
                self.store.save_resource("worktrunk.activity", &ledger)?;
                Ok(())
            })();
            if let Err(error) = result {
                failure = Some(error);
            }
        }
        failure.map_or(Ok(()), Err)
    }
}
