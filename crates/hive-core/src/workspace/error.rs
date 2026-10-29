//! Stable diagnostic codes can be adapted at the binary edge without coupling core to miette.

use serde::{Deserialize, Serialize};

use super::{RetryDisposition, WorkspaceState};
use crate::identity::{NodeId, OperationId, ProjectId, WorkspaceId};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CleanupRefusal {
    ExternalOwnership,
    UnverifiedOwnership,
    Dirty,
    UnknownCleanliness,
    InUse,
    UnknownUse,
    IdentityConflict,
}

/// Controlled reasons avoid exposing credentials or arbitrary subprocess output.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProvisionFailure {
    SourceUnavailable,
    RefUnavailable,
    BranchConflict,
    PathConflict,
    PermissionDenied,
    Interrupted,
    BackendUnavailable,
}

/// Ordinary removal failures are distinct from safety refusals and uncertain hooks.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CleanupFailure {
    PermissionDenied,
    Interrupted,
    BackendUnavailable,
    RemovalFailed,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum WorkspaceError {
    #[error("unknown project {project}; register the project or select a registered identifier")]
    UnknownProject { project: ProjectId },
    #[error("execution node {node} is unavailable; reconnect it or select an available node")]
    UnavailableNode { node: NodeId },
    #[error("invalid Git ref; supply a nonempty ref and resolve it on the owning node")]
    InvalidRef,
    #[error("invalid path for node {node}; supply an absolute path using that node's syntax")]
    InvalidPath { node: NodeId },
    #[error(
        "workspace belongs to node {expected}, not {actual}; route the operation to its owning node"
    )]
    NodeMismatch { expected: NodeId, actual: NodeId },
    #[error(
        "invalid workspace intent; match existing paths to their node and give ephemeral workspaces an expiry or idle retention rule"
    )]
    InvalidIntent,
    #[error("invalid persisted workspace state; reconcile the record with its owning backend")]
    InvalidRecord,
    #[error(
        "cannot transition workspace {workspace} from {state:?}; inspect its lifecycle before retrying"
    )]
    InvalidTransition {
        workspace: WorkspaceId,
        state: WorkspaceState,
    },
    #[error(
        "provisioning workspace {workspace} failed ({reason:?}); follow {retry:?} retry guidance"
    )]
    Provisioning {
        workspace: WorkspaceId,
        reason: ProvisionFailure,
        retry: RetryDisposition,
    },
    #[error(
        "refusing cleanup of workspace {workspace} ({reason:?}); preserve the resource and resolve the safety condition"
    )]
    UnsafeCleanup {
        workspace: WorkspaceId,
        reason: CleanupRefusal,
    },
    #[error(
        "cleanup of workspace {workspace} failed ({reason:?}); follow {retry:?} retry guidance"
    )]
    Cleanup {
        workspace: WorkspaceId,
        reason: CleanupFailure,
        retry: RetryDisposition,
    },
    #[error(
        "operation {operation} was reused with different intent; restore the original request or allocate a new operation after reconciliation"
    )]
    RetryConflict { operation: OperationId },
    #[error(
        "operation {operation} has uncertain effects; inspect the owning backend before retrying"
    )]
    UncertainOperation { operation: OperationId },
    #[error("operation {operation} is not safely repeatable; reconcile external effects manually")]
    NonIdempotent { operation: OperationId },
    #[error(
        "WorkTrunk is required but {availability:?}; install a compatible wt on the execution node or explicitly disable the integration"
    )]
    WorktrunkRequired {
        availability: super::integration::WorktrunkAvailability,
    },
}

impl WorkspaceError {
    /// Public identifiers: changing these strings is a breaking diagnostic change.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnknownProject { .. } => "hive::workspace::unknown_project",
            Self::UnavailableNode { .. } => "hive::workspace::unavailable_node",
            Self::InvalidRef => "hive::workspace::invalid_ref",
            Self::InvalidPath { .. } => "hive::workspace::invalid_path",
            Self::NodeMismatch { .. } => "hive::workspace::node_mismatch",
            Self::InvalidIntent => "hive::workspace::invalid_intent",
            Self::InvalidRecord => "hive::workspace::invalid_record",
            Self::InvalidTransition { .. } => "hive::workspace::invalid_transition",
            Self::Provisioning { .. } => "hive::workspace::provisioning_failed",
            Self::UnsafeCleanup { .. } => "hive::workspace::unsafe_cleanup",
            Self::Cleanup { .. } => "hive::workspace::cleanup_failed",
            Self::RetryConflict { .. } => "hive::workspace::retry_conflict",
            Self::UncertainOperation { .. } => "hive::workspace::uncertain_operation",
            Self::NonIdempotent { .. } => "hive::workspace::non_idempotent",
            Self::WorktrunkRequired { .. } => "hive::workspace::worktrunk_required",
        }
    }

    #[must_use]
    pub fn retry(&self) -> RetryDisposition {
        match self {
            Self::Provisioning { retry, .. } | Self::Cleanup { retry, .. } => *retry,
            Self::UnavailableNode { .. } => RetryDisposition::SameOperation,
            Self::UncertainOperation { .. } => RetryDisposition::InspectFirst,
            _ => RetryDisposition::ManualIntervention,
        }
    }

    #[must_use]
    pub fn help(&self) -> &'static str {
        match self.retry() {
            RetryDisposition::SameOperation => {
                "Correct the reported condition and retry with the same operation ID and identical intent."
            }
            RetryDisposition::InspectFirst => {
                "Inspect and reconcile on the owning node before repeating any side effects."
            }
            RetryDisposition::ManualIntervention => {
                "Resolve the reported condition explicitly; do not blindly repeat the operation."
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domain_diagnostic_codes_are_stable_and_every_failure_has_retry_guidance() {
        let workspace = WorkspaceId::new();
        let operation = OperationId::new();
        let cases = [
            (
                WorkspaceError::UnknownProject {
                    project: ProjectId::new(),
                },
                "unknown_project",
            ),
            (
                WorkspaceError::UnavailableNode {
                    node: NodeId::new(),
                },
                "unavailable_node",
            ),
            (WorkspaceError::InvalidRef, "invalid_ref"),
            (
                WorkspaceError::InvalidPath {
                    node: NodeId::new(),
                },
                "invalid_path",
            ),
            (
                WorkspaceError::NodeMismatch {
                    expected: NodeId::new(),
                    actual: NodeId::new(),
                },
                "node_mismatch",
            ),
            (WorkspaceError::InvalidIntent, "invalid_intent"),
            (WorkspaceError::InvalidRecord, "invalid_record"),
            (
                WorkspaceError::InvalidTransition {
                    workspace: workspace.clone(),
                    state: WorkspaceState::Removed,
                },
                "invalid_transition",
            ),
            (
                WorkspaceError::Provisioning {
                    workspace: workspace.clone(),
                    reason: ProvisionFailure::Interrupted,
                    retry: RetryDisposition::InspectFirst,
                },
                "provisioning_failed",
            ),
            (
                WorkspaceError::UnsafeCleanup {
                    workspace: workspace.clone(),
                    reason: CleanupRefusal::Dirty,
                },
                "unsafe_cleanup",
            ),
            (
                WorkspaceError::Cleanup {
                    workspace,
                    reason: CleanupFailure::PermissionDenied,
                    retry: RetryDisposition::SameOperation,
                },
                "cleanup_failed",
            ),
            (
                WorkspaceError::RetryConflict {
                    operation: operation.clone(),
                },
                "retry_conflict",
            ),
            (
                WorkspaceError::UncertainOperation {
                    operation: operation.clone(),
                },
                "uncertain_operation",
            ),
            (
                WorkspaceError::NonIdempotent { operation },
                "non_idempotent",
            ),
            (
                WorkspaceError::WorktrunkRequired {
                    availability: super::super::integration::WorktrunkAvailability::Unavailable,
                },
                "worktrunk_required",
            ),
        ];
        for (error, kind) in cases {
            assert_eq!(error.code(), format!("hive::workspace::{kind}"));
            assert!(!error.to_string().is_empty());
            assert!(!error.help().is_empty());
            if kind == "cleanup_failed" {
                assert_eq!(error.retry(), RetryDisposition::SameOperation);
                assert!(error.help().contains("same operation ID"));
                assert!(error.to_string().contains("PermissionDenied"));
            }
        }
    }
}
