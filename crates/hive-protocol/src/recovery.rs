//! Stable, controlled recovery diagnostics shared by gateway clients.

#[derive(Clone, Copy, Debug, thiserror::Error)]
pub enum RecoveryError {
    #[error("prompt outcome is uncertain; Hive will not dispatch this key again")]
    Uncertain,
    #[error("prompt was not dispatched; this key remains reserved")]
    NotDispatched,
    #[error("prompt key was used with different content")]
    KeyConflict,
    #[error("session recovery requires explicit acknowledgement before new work")]
    Required,
    #[error("cannot persist prompt evidence; new work is blocked")]
    Storage,
}

impl RecoveryError {
    pub fn code(self) -> &'static str {
        match self {
            Self::Uncertain => "hive::prompt::outcome_uncertain",
            Self::NotDispatched => "hive::prompt::not_dispatched",
            Self::KeyConflict => "hive::prompt::key_conflict",
            Self::Required => "hive::session::recovery_required",
            Self::Storage => "hive::prompt::journal_failed",
        }
    }

    pub fn help(self) -> &'static str {
        match self {
            Self::Uncertain | Self::Required | Self::Storage => {
                "Observe the original session and inspect external effects, then acknowledge recovery on session/resume. Acknowledgement does not establish completion or transcript completeness."
            }
            Self::NotDispatched => {
                "Edit or submit new intent with a new key; this reserved key will never execute."
            }
            Self::KeyConflict => {
                "Retrieve the original receipt using the original content, or use a new key for new intent."
            }
        }
    }

    pub fn wire(self, recovery_required: bool) -> agent_client_protocol::Error {
        let error = if matches!(self, Self::KeyConflict) {
            agent_client_protocol::Error::invalid_params()
        } else {
            agent_client_protocol::Error::invalid_request()
        };
        error.data(serde_json::json!({"hive": {
            "version": 1, "code": self.code(), "message": self.to_string(),
            "help": self.help(), "recoveryRequired": recovery_required,
            "retry": "receipt_only"
        }}))
    }
}
