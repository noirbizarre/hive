//! Workspace intent, observed resources, and checked lifecycle transitions.
//!
//! These contracts perform no filesystem or Git operations. Backend inspection on
//! the owning node is authoritative for refs, resolved paths, ownership, and safety.

pub mod backend;
mod error;
pub mod integration;

#[cfg(test)]
mod tests;

pub use error::{CleanupRefusal, WorkspaceError};

use serde::{Deserialize, Serialize};

use crate::identity::{NodeId, OperationId, ProjectId, RepositoryId, WorkspaceId};

/// Paths use the owning node's syntax, never the initiating host's `Path::is_absolute`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PathSyntax {
    Posix,
    Windows,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "NodePathRecord", into = "NodePathRecord")]
pub struct NodePath(NodePathRecord);

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NodePathRecord {
    node: NodeId,
    syntax: PathSyntax,
    path: String,
}

impl NodePath {
    /// Validate only absolute syntax. Existence, symlinks and containment need inspection.
    pub fn new(node: NodeId, syntax: PathSyntax, path: String) -> Result<Self, WorkspaceError> {
        Self::try_from(NodePathRecord { node, syntax, path })
    }

    #[must_use]
    pub fn node(&self) -> &NodeId {
        &self.0.node
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0.path
    }

    #[must_use]
    pub fn syntax(&self) -> PathSyntax {
        self.0.syntax
    }
}

impl TryFrom<NodePathRecord> for NodePath {
    type Error = WorkspaceError;

    fn try_from(record: NodePathRecord) -> Result<Self, Self::Error> {
        let path = record.path.as_bytes();
        let absolute = match record.syntax {
            PathSyntax::Posix => path.starts_with(b"/"),
            PathSyntax::Windows => {
                let drive = path.len() >= 3
                    && path[0].is_ascii_alphabetic()
                    && path[1] == b':'
                    && matches!(path[2], b'\\' | b'/');
                // A UNC path needs both a server and a share, not just two slashes.
                let unc = record.path.strip_prefix("\\\\").is_some_and(|rest| {
                    let mut parts = rest.split('\\');
                    parts.next().is_some_and(|p| !p.is_empty())
                        && parts.next().is_some_and(|p| !p.is_empty())
                });
                drive || unc
            }
        };
        if !absolute || record.path.contains('\0') {
            return Err(WorkspaceError::InvalidPath { node: record.node });
        }
        Ok(Self(record))
    }
}

impl From<NodePath> for NodePathRecord {
    fn from(path: NodePath) -> Self {
        path.0
    }
}

/// A syntactically nonempty ref expression; the backend must resolve it using Git.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct GitRef(String);

impl TryFrom<String> for GitRef {
    type Error = WorkspaceError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        // Reject option-like inputs before they can reach a subprocess argument list.
        if value.is_empty()
            || value.starts_with('-')
            || value.chars().any(char::is_control)
            || value.chars().all(char::is_whitespace)
        {
            Err(WorkspaceError::InvalidRef)
        } else {
            Ok(Self(value))
        }
    }
}

impl GitRef {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<GitRef> for String {
    fn from(value: GitRef) -> Self {
        value.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ownership {
    External,
    HiveManaged,
}

/// A missing branch requests a deterministic task branch, not the repository's shared branch.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitIntent {
    pub base_ref: GitRef,
    pub branch: Option<GitRef>,
}

impl GitIntent {
    #[must_use]
    pub fn effective_branch(&self, workspace: &WorkspaceId) -> String {
        self.branch.as_ref().map_or_else(
            || format!("hive/{workspace}"),
            |branch| branch.as_str().to_owned(),
        )
    }
}

/// Explicit backing prevents a temporary policy from silently changing isolation mechanics.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ManagedBacking {
    Worktree {
        repository: RepositoryId,
        git: GitIntent,
    },
    Clone {
        git: GitIntent,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "policy", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkspacePolicy {
    Existing {
        directory: NodePath,
    },
    Worktree {
        repository: RepositoryId,
        git: GitIntent,
    },
    Clone {
        git: GitIntent,
    },
    Ephemeral {
        backing: ManagedBacking,
    },
}

/// Retention eligibility never proves cleanup safety. Times are Unix seconds.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Retention {
    Retain,
    Until {
        expires_at: u64,
    },
    Idle {
        last_used_at: u64,
        idle_seconds: u64,
    },
}

impl Retention {
    #[must_use]
    pub fn eligible_at(&self, now: u64) -> bool {
        match self {
            Self::Retain => false,
            Self::Until { expires_at } => now >= *expires_at,
            Self::Idle {
                last_used_at,
                idle_seconds,
            } => {
                // Overflow must retain data, not turn an enormous retention period into immediate expiry.
                last_used_at
                    .checked_add(*idle_seconds)
                    .is_some_and(|expiry| now >= expiry)
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceIntent {
    pub project: ProjectId,
    pub node: NodeId,
    pub policy: WorkspacePolicy,
    pub retention: Retention,
}

impl WorkspaceIntent {
    pub fn validate(&self) -> Result<(), WorkspaceError> {
        if let WorkspacePolicy::Existing { directory } = &self.policy {
            self.check_node(directory.node())?;
        }
        if matches!(self.policy, WorkspacePolicy::Ephemeral { .. })
            && matches!(self.retention, Retention::Retain)
        {
            return Err(WorkspaceError::InvalidIntent);
        }
        Ok(())
    }

    pub fn check_node(&self, node: &NodeId) -> Result<(), WorkspaceError> {
        if node == &self.node {
            Ok(())
        } else {
            Err(WorkspaceError::NodeMismatch {
                expected: self.node.clone(),
                actual: node.clone(),
            })
        }
    }

    #[must_use]
    pub fn ownership(&self) -> Ownership {
        if matches!(self.policy, WorkspacePolicy::Existing { .. }) {
            Ownership::External
        } else {
            Ownership::HiveManaged
        }
    }
}

/// Opaque integration identity, interpreted only by the backend registry.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BackendId(pub String);

/// Backend-authoritative observations, persisted verbatim after provisioning.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceLocation {
    pub root: NodePath,
    pub working_directory: NodePath,
    pub backend: BackendId,
    pub resolved_base: Option<GitRef>,
    pub branch: Option<GitRef>,
    /// Opaque object ID: object format validation belongs to the selected Git backend.
    pub commit: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetryDisposition {
    SameOperation,
    InspectFirst,
    ManualIntervention,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceState {
    Requested,
    Provisioning,
    Ready,
    Failed,
    Cleaning,
    CleanupFailed,
    Removed,
}

/// Events are reported by orchestration after authoritative backend results.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspaceEvent {
    Provision {
        operation: OperationId,
        backend: BackendId,
    },
    Provisioned {
        location: WorkspaceLocation,
    },
    ProvisionFailed {
        retry: RetryDisposition,
        location: Option<WorkspaceLocation>,
    },
    Cleanup {
        operation: OperationId,
    },
    Cleaned,
    CleanupFailed {
        retry: RetryDisposition,
    },
    /// Inspection permits resuming the same operation, not inventing a new attempt key.
    ReconciledRetry,
    /// Inspection (and any necessary manual reconciliation) has established that
    /// abandoning provision and removing its partial resource is safe. This does
    /// not authorize repeating non-idempotent provisioning hooks.
    ReconciledCleanup {
        operation: OperationId,
    },
    ReconciledReady {
        location: WorkspaceLocation,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "WorkspaceRecord", into = "WorkspaceRecord")]
pub struct Workspace(WorkspaceRecord);

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceRecord {
    id: WorkspaceId,
    intent: WorkspaceIntent,
    state: WorkspaceState,
    operation: Option<OperationId>,
    backend: Option<BackendId>,
    retry: Option<RetryDisposition>,
    location: Option<WorkspaceLocation>,
}

impl Workspace {
    pub fn new(id: WorkspaceId, intent: WorkspaceIntent) -> Result<Self, WorkspaceError> {
        Self::try_from(WorkspaceRecord {
            id,
            intent,
            state: WorkspaceState::Requested,
            operation: None,
            backend: None,
            retry: None,
            location: None,
        })
    }

    #[must_use]
    pub fn id(&self) -> &WorkspaceId {
        &self.0.id
    }
    #[must_use]
    pub fn intent(&self) -> &WorkspaceIntent {
        &self.0.intent
    }
    #[must_use]
    pub fn state(&self) -> WorkspaceState {
        self.0.state
    }
    #[must_use]
    pub fn operation(&self) -> Option<&OperationId> {
        self.0.operation.as_ref()
    }
    /// Retained even when provisioning failed before discovering a path.
    #[must_use]
    pub fn backend(&self) -> Option<&BackendId> {
        self.0.backend.as_ref()
    }
    #[must_use]
    pub fn retry(&self) -> Option<RetryDisposition> {
        self.0.retry
    }
    #[must_use]
    pub fn location(&self) -> Option<&WorkspaceLocation> {
        self.0.location.as_ref()
    }

    /// Invalid events leave the entire record unchanged, including its retry key.
    pub fn transition(&mut self, event: WorkspaceEvent) -> Result<(), WorkspaceError> {
        use WorkspaceEvent as E;
        use WorkspaceState as S;
        let mut next = self.0.clone();
        match (next.state, event) {
            (S::Requested, E::Provision { operation, backend }) => {
                next.state = S::Provisioning;
                next.operation = Some(operation);
                next.backend = Some(backend);
            }
            (S::Failed, E::Provision { operation, backend })
                if next.retry == Some(RetryDisposition::SameOperation)
                    && next.operation.as_ref() == Some(&operation)
                    && next.backend.as_ref() == Some(&backend) =>
            {
                next.state = S::Provisioning;
                next.retry = None;
            }
            (S::Provisioning, E::Provisioned { location }) => {
                next.state = S::Ready;
                next.location = Some(location);
            }
            (S::Provisioning, E::ProvisionFailed { retry, location }) => {
                next.state = S::Failed;
                next.retry = Some(retry);
                // A failed retry without new observations must not erase a partial resource's path.
                if location.is_some() {
                    next.location = location;
                }
            }
            (S::Ready | S::Failed, E::Cleanup { operation })
                if (next.retry.is_none()
                    || next.retry == Some(RetryDisposition::SameOperation))
                    && next.operation.as_ref() != Some(&operation) =>
            {
                next.state = S::Cleaning;
                next.operation = Some(operation);
                next.retry = None;
            }
            (S::CleanupFailed, E::Cleanup { operation })
                if next.retry == Some(RetryDisposition::SameOperation)
                    && next.operation.as_ref() == Some(&operation) =>
            {
                next.state = S::Cleaning;
                next.retry = None;
            }
            (S::Failed, E::ReconciledCleanup { operation })
                if next.operation.as_ref() != Some(&operation) =>
            {
                next.state = S::Cleaning;
                next.operation = Some(operation);
                next.retry = None;
            }
            (S::Cleaning, E::Cleaned) => {
                next.state = S::Removed;
            }
            (S::Cleaning, E::CleanupFailed { retry }) => {
                next.state = S::CleanupFailed;
                next.retry = Some(retry);
            }
            (S::Provisioning | S::Cleaning | S::Failed | S::CleanupFailed, E::ReconciledRetry)
                if next.retry != Some(RetryDisposition::ManualIntervention) =>
            {
                // A crash during an in-flight operation is uncertain even without a failure record.
                next.state = if matches!(next.state, S::Provisioning | S::Failed) {
                    S::Failed
                } else {
                    S::CleanupFailed
                };
                next.retry = Some(RetryDisposition::SameOperation);
            }
            (S::Provisioning | S::Failed | S::CleanupFailed, E::ReconciledReady { location }) => {
                next.state = S::Ready;
                next.retry = None;
                next.location = Some(location);
            }
            _ => {
                return Err(WorkspaceError::InvalidTransition {
                    workspace: self.0.id.clone(),
                    state: self.0.state,
                });
            }
        }
        *self = Self::try_from(next)?;
        Ok(())
    }
}

impl TryFrom<WorkspaceRecord> for Workspace {
    type Error = WorkspaceError;

    fn try_from(record: WorkspaceRecord) -> Result<Self, Self::Error> {
        record.intent.validate()?;
        if let Some(location) = &record.location {
            record.intent.check_node(location.root.node())?;
            record
                .intent
                .check_node(location.working_directory.node())?;
            if Some(&location.backend) != record.backend.as_ref() {
                return Err(WorkspaceError::InvalidRecord);
            }
        }
        let needs_retry = matches!(
            record.state,
            WorkspaceState::Failed | WorkspaceState::CleanupFailed
        );
        let invalid = (record.state == WorkspaceState::Requested
            && (record.operation.is_some()
                || record.location.is_some()
                || record.backend.is_some()))
            || (record.state != WorkspaceState::Requested
                && (record.operation.is_none() || record.backend.is_none()))
            || record
                .backend
                .as_ref()
                .is_some_and(|backend| backend.0.trim().is_empty())
            || (record.state == WorkspaceState::Ready && record.location.is_none())
            || (needs_retry != record.retry.is_some());
        if invalid {
            return Err(WorkspaceError::InvalidRecord);
        }
        Ok(Self(record))
    }
}

impl From<Workspace> for WorkspaceRecord {
    fn from(workspace: Workspace) -> Self {
        workspace.0
    }
}
