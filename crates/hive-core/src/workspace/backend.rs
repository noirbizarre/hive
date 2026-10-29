//! Runtime-independent boundary for provisioning, inspection and safe removal.

use std::{future::Future, pin::Pin};

use serde::{Deserialize, Serialize};

use super::{
    BackendId, CleanupRefusal, Ownership, WorkspaceError, WorkspaceIntent, WorkspaceLocation,
};
use crate::identity::{NodeId, OperationId, WorkspaceId};

pub use super::error::{CleanupFailure, ProvisionFailure};

/// Object-safe async dispatch without choosing an executor for library consumers.
pub type BackendFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, WorkspaceError>> + Send + 'a>>;

/// One durable provisioning request. Equality includes all intent, not just the workspace ID.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProvisionRequest {
    pub operation: OperationId,
    pub workspace: WorkspaceId,
    /// Pin before external effects, so interrupted operations cannot switch integrations.
    pub backend: BackendId,
    pub intent: WorkspaceIntent,
}

impl ProvisionRequest {
    /// Backends call this against the original durable request before repeating side effects.
    pub fn check_retry(&self, original: &Self) -> Result<(), WorkspaceError> {
        if self == original {
            Ok(())
        } else {
            Err(WorkspaceError::RetryConflict {
                operation: self.operation.clone(),
            })
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProvisionOutcome {
    Created,
    AlreadySatisfied,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProvisionResult {
    pub workspace: WorkspaceId,
    pub operation: OperationId,
    pub location: WorkspaceLocation,
    pub outcome: ProvisionOutcome,
}

/// Persisted identity is used for dispatch even if global integration settings change.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ResourceTarget {
    pub workspace: WorkspaceId,
    pub node: NodeId,
    pub backend: BackendId,
    pub ownership: Ownership,
    /// May be absent after an interrupted provision: reconcile using stable identity.
    pub location: Option<WorkspaceLocation>,
}

impl ResourceTarget {
    /// Validate routing before accessing the filesystem on an execution node.
    pub fn check_node(&self, node: &NodeId) -> Result<(), WorkspaceError> {
        if node != &self.node {
            return Err(WorkspaceError::NodeMismatch {
                expected: self.node.clone(),
                actual: node.clone(),
            });
        }
        if let Some(location) = &self.location {
            for path in [&location.root, &location.working_directory] {
                if path.node() != node {
                    return Err(WorkspaceError::NodeMismatch {
                        expected: node.clone(),
                        actual: path.node().clone(),
                    });
                }
            }
            if location.backend != self.backend {
                return Err(WorkspaceError::UnsafeCleanup {
                    workspace: self.workspace.clone(),
                    reason: CleanupRefusal::IdentityConflict,
                });
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cleanliness {
    Clean,
    Dirty,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceUse {
    Idle,
    Active,
    Unknown,
}

/// Evidence is descriptive, not a deletion capability. Cleanup must obtain it afresh.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SafetyEvidence {
    pub ownership_verified: bool,
    pub cleanliness: Cleanliness,
    pub usage: ResourceUse,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "presence", rename_all = "snake_case")]
pub enum Inspection {
    /// Absence must be established by resource identity, not just a missing expected path.
    Absent,
    Present {
        location: WorkspaceLocation,
        safety: SafetyEvidence,
    },
    Partial {
        location: Option<WorkspaceLocation>,
        safety: SafetyEvidence,
    },
    IdentityConflict,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CleanupMode {
    PreserveChanges,
    DiscardChanges,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CleanupRequest {
    pub operation: OperationId,
    pub target: ResourceTarget,
    pub mode: CleanupMode,
}

impl CleanupRequest {
    /// Replay the original durable request verbatim, including its location snapshot.
    /// Newly discovered locations are backend observations, not changes to deletion
    /// intent: resolve them internally by identity instead of rebuilding this request.
    pub fn check_retry(&self, original: &Self) -> Result<(), WorkspaceError> {
        if self == original {
            Ok(())
        } else {
            Err(WorkspaceError::RetryConflict {
                operation: self.operation.clone(),
            })
        }
    }

    /// Call inside the backend's mutation guard with freshly inspected evidence.
    /// External records can be detached separately, but this API never deletes them.
    pub fn check_safety(&self, evidence: &SafetyEvidence) -> Result<(), WorkspaceError> {
        self.target.check_node(&self.target.node)?;
        let reason = if self.target.ownership == Ownership::External {
            Some(CleanupRefusal::ExternalOwnership)
        } else if !evidence.ownership_verified {
            Some(CleanupRefusal::UnverifiedOwnership)
        } else if evidence.usage == ResourceUse::Active {
            Some(CleanupRefusal::InUse)
        } else if evidence.usage == ResourceUse::Unknown {
            Some(CleanupRefusal::UnknownUse)
        } else if evidence.cleanliness == Cleanliness::Unknown {
            Some(CleanupRefusal::UnknownCleanliness)
        } else if evidence.cleanliness == Cleanliness::Dirty
            && self.mode == CleanupMode::PreserveChanges
        {
            Some(CleanupRefusal::Dirty)
        } else {
            None
        };
        match reason {
            Some(reason) => Err(WorkspaceError::UnsafeCleanup {
                workspace: self.target.workspace.clone(),
                reason,
            }),
            None => Ok(()),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CleanupOutcome {
    Removed,
    AlreadyAbsent,
}

/// Implementations execute on the owning node and serialize mutations per resource.
///
/// Before mutation, validate intent/routing, resolve project/repository identity and
/// verify backend provenance. Persist operation ID + complete intent before external
/// effects. Repeated identical requests must reconcile, never allocate another resource;
/// reused keys with changed intent return `RetryConflict`. A key is global across
/// mutation kinds: it cannot be reused for cleanup after provisioning.
///
/// Inspection is read-only and safe to repeat. After interruption or a lost response,
/// inspect by identity before retrying. Report `UncertainOperation` or `NonIdempotent`
/// when hooks/external effects cannot safely be repeated; never promise exactly-once hooks.
///
/// Cleanup rechecks ownership, dirty/untracked files and active leases under the same
/// mutation guard as deletion. Never trust caller-supplied paths or serialized evidence.
/// An absent result requires reconciliation by identity, not guessing a path template.
/// Actual paths returned by integrations must be discovered and persisted verbatim.
pub trait WorkspaceBackend: Send + Sync {
    fn provision(&self, request: ProvisionRequest) -> BackendFuture<'_, ProvisionResult>;
    fn inspect(&self, target: ResourceTarget) -> BackendFuture<'_, Inspection>;
    fn cleanup(&self, request: CleanupRequest) -> BackendFuture<'_, CleanupOutcome>;
}
