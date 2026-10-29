//! Transport-independent identities and domain contracts for Hive orchestration.

pub mod identity;
pub mod project;
pub mod workspace;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct SessionId(String);

impl SessionId {
    #[must_use]
    pub fn new() -> Self {
        Self(format!("hive-{}", uuid::Uuid::new_v4()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Idle,
    Running,
    Closed,
    Failed,
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("unknown Hive session {0}; list sessions and select an active session")]
    Unknown(String),
    #[error("agent connection failed: {0}; check the provider command and agent logs")]
    Agent(String),
    #[error("operation is unavailable for this provider: {0}; inspect provider capabilities")]
    Unsupported(String),
}
