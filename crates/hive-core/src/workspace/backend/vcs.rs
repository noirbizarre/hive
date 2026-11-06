//! Working-copy operations are SCM responsibilities, independent of provisioning and transport.

use serde::{Deserialize, Serialize};

use super::RepositoryHandle;
use crate::workspace::NodePath;

pub struct Target<'a> {
    pub repository: &'a RepositoryHandle,
    pub root: &'a NodePath,
    /// Recheck attachment authority immediately before staging or snapshotting can have effects.
    pub authorize_mutation: &'a dyn Fn() -> std::result::Result<(), Error>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Status {},
    Diff {
        #[serde(default)]
        view: DiffView,
        #[serde(default = "default_bytes")]
        max_bytes: usize,
    },
    Commit {
        message: String,
        inclusion: Inclusion,
        #[serde(default)]
        untracked: Vec<String>,
        #[serde(default)]
        ignored: Vec<String>,
        #[serde(default)]
        expected_state: Option<String>,
    },
    Log {
        #[serde(default = "default_limit")]
        limit: usize,
    },
}

pub const fn default_bytes() -> usize {
    64 * 1024
}
pub const fn default_limit() -> usize {
    20
}

impl Request {
    pub fn mutates(&self) -> bool {
        matches!(self, Self::Commit { .. })
    }
    pub fn validate(&self) -> std::result::Result<(), Error> {
        match self {
            Self::Diff { max_bytes, .. } if !(1..=256 * 1024).contains(max_bytes) => {
                Err(Error::Arguments)
            }
            Self::Log { limit } if !(1..=100).contains(limit) => Err(Error::Arguments),
            Self::Commit {
                message,
                inclusion,
                untracked,
                ignored,
                ..
            } if message.trim().is_empty()
                || message.len() > 16 * 1024
                || message.contains('\0')
                || untracked.len() + ignored.len() > 1000
                || (*inclusion == Inclusion::StagedOnly
                    && (!untracked.is_empty() || !ignored.is_empty())) =>
            {
                Err(Error::Arguments)
            }
            _ => Ok(()),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffView {
    Staged,
    Unstaged,
    #[default]
    WorkingCopy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Inclusion {
    StagedOnly,
    TrackedWorkingCopy,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Identity {
    pub commit_id: String,
    pub change_id: Option<String>,
    pub operation_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileState {
    pub path: String,
    pub original_path: Option<String>,
    pub index: Option<String>,
    pub working_copy: Option<String>,
    pub untracked: bool,
    pub ignored: bool,
    pub conflict: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct State {
    pub identity: Identity,
    /// Native Git branch, or null for detached Git and JJ's change-oriented workspace.
    pub branch: Option<String>,
    pub state_token: String,
    pub dirty: bool,
    /// JJ can have recorded changes even when every physical file matches its recorded tree.
    pub recorded_change: bool,
    pub conflicts: bool,
    pub files: Vec<FileState>,
    pub complete: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffSection {
    pub layer: String,
    pub text: String,
    pub truncated: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Change {
    pub identity: Identity,
    pub message: String,
    pub message_truncated: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Result {
    pub outcome: String,
    pub state: State,
    pub recorded: Option<Change>,
    pub diff: Vec<DiffSection>,
    pub history: Vec<Change>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("The VCS operation or inclusion mode is unsupported by this backend")]
    Unsupported,
    #[error("Invalid VCS arguments; check the advertised schema and literal relative file paths")]
    Arguments,
    #[error("There are no selected changes to record")]
    Empty,
    #[error(
        "Resolve workspace conflicts or finish the in-progress SCM operation before committing"
    )]
    Conflict,
    #[error(
        "Workspace state changed concurrently; inspect status and retry with its current state token"
    )]
    Concurrent,
    #[error(
        "The backend could not inspect this workspace; check native SCM configuration on the owning node"
    )]
    Backend,
    #[error(
        "A mutation was dispatched and may have partial or complete effects; inspect status and log before retrying"
    )]
    Uncertain,
    #[error("Session authority ended before mutation dispatch; no mutation was dispatched")]
    Cancelled,
}
