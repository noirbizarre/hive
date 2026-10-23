//! Typed, actionable failures of the OpenCode adapter.
//!
//! Every variant carries a stable `hive::opencode::<kind>` code. Users grep for these, so a
//! rename is a breaking change (see AGENTS.md). No message may contain the server URL, headers,
//! credentials or response bodies: those routinely hold secrets, and the text is shown to
//! remote Hive clients and persisted in the session catalog.

use miette::Diagnostic;

/// Code strings, also matched by the recovery classifier in `lib.rs`.
///
/// Kept as constants so the classifier and the diagnostics cannot drift apart: a renamed code
/// would otherwise silently turn a `lost` session into an endlessly retried one.
pub(crate) mod code {
    pub const UNREACHABLE: &str = "hive::opencode::unreachable";
    pub const UNAUTHORIZED: &str = "hive::opencode::unauthorized";
    pub const UNSUPPORTED_VERSION: &str = "hive::opencode::unsupported_version";
    pub const API_UNAVAILABLE: &str = "hive::opencode::api_unavailable";
    pub const SESSION_NOT_FOUND: &str = "hive::opencode::session_not_found";
    pub const ALREADY_ADOPTED: &str = "hive::opencode::already_adopted";
    pub const ADOPTION_IN_PROGRESS: &str = "hive::opencode::adoption_in_progress";
    pub const EXTERNALLY_CONTROLLED: &str = "hive::opencode::externally_controlled";
    pub const STREAM_LOST: &str = "hive::opencode::stream_lost";
    pub const DIRECTORY_MISMATCH: &str = "hive::opencode::directory_mismatch";
    pub const MISSING_CREDENTIALS: &str = "hive::opencode::missing_credentials";
    pub const UNSUPPORTED_PROMPT: &str = "hive::opencode::unsupported_prompt";
    pub const HTTP_STATUS: &str = "hive::opencode::http_status";
    pub const PROTOCOL: &str = "hive::opencode::protocol";
}

#[derive(Clone, Debug, thiserror::Error, Diagnostic, PartialEq, Eq)]
pub(crate) enum OpencodeError {
    #[error("the OpenCode server did not answer ({reason})")]
    #[diagnostic(
        code(hive::opencode::unreachable),
        help(
            "check that `opencode serve` (or the OpenCode TUI with --port) is running and that the profile endpoint is correct"
        )
    )]
    Unreachable { reason: &'static str },

    #[error("the OpenCode server rejected the credentials")]
    #[diagnostic(
        code(hive::opencode::unauthorized),
        help(
            "set the profile's username/password_env to match OPENCODE_SERVER_USERNAME/OPENCODE_SERVER_PASSWORD on the server"
        )
    )]
    Unauthorized,

    #[error("OpenCode {found} is not supported (minimum {minimum})")]
    #[diagnostic(
        code(hive::opencode::unsupported_version),
        help(
            "upgrade OpenCode; see the OpenCode server capability matrix in docs/acp-compatibility.md for tested versions"
        )
    )]
    UnsupportedVersion {
        found: String,
        minimum: &'static str,
    },

    #[error("the OpenCode server does not advertise {what}")]
    #[diagnostic(
        code(hive::opencode::api_unavailable),
        help(
            "Hive refuses to fall back to a new session; upgrade OpenCode or pick a server that provides this API"
        )
    )]
    ApiUnavailable { what: &'static str },

    #[error("saved session not found in OpenCode: {id}")]
    #[diagnostic(
        code(hive::opencode::session_not_found),
        help(
            "the session was deleted or lives in another OpenCode project; list sessions with GET /hive/v1/providers/<agent>/native-sessions?directory=<cwd>"
        )
    )]
    SessionNotFound { id: String },

    #[error("OpenCode session {native_id} is already attached as Hive session {hive_id}")]
    #[diagnostic(
        code(hive::opencode::already_adopted),
        help("resume the existing Hive session instead of adopting it again")
    )]
    AlreadyAdopted { native_id: String, hive_id: String },

    #[error("OpenCode session {native_id} is already being adopted by another request")]
    #[diagnostic(
        code(hive::opencode::adoption_in_progress),
        help("wait for that adoption to finish, then resume the resulting Hive session")
    )]
    AdoptionInProgress { native_id: String },

    #[error("the OpenCode session is busy with work Hive did not start")]
    #[diagnostic(
        code(hive::opencode::externally_controlled),
        help(
            "another OpenCode client is driving this session; wait for it to go idle or cancel its turn from Hive"
        )
    )]
    ExternallyControlled,

    #[error("the connection to the OpenCode event stream was lost")]
    #[diagnostic(
        code(hive::opencode::stream_lost),
        help("Hive is reconnecting; if this persists check the OpenCode server and restart Hive")
    )]
    StreamLost,

    #[error("OpenCode session lives in {native}, not the requested {requested}")]
    #[diagnostic(
        code(hive::opencode::directory_mismatch),
        help(
            "pass the session's own directory as cwd; a different cwd would act on the wrong project"
        )
    )]
    DirectoryMismatch { native: String, requested: String },

    #[error("environment variable {var} named by password_env is not set")]
    #[diagnostic(
        code(hive::opencode::missing_credentials),
        help(
            "export it in the environment of the Hive daemon, or remove password_env for an unauthenticated server"
        )
    )]
    MissingCredentials { var: String },

    #[error("the OpenCode adapter only forwards text prompts")]
    #[diagnostic(
        code(hive::opencode::unsupported_prompt),
        help("send text content blocks only")
    )]
    UnsupportedPrompt,

    #[error("the OpenCode server answered HTTP {status}")]
    #[diagnostic(
        code(hive::opencode::http_status),
        help(
            "check the OpenCode server log; Hive does not surface response bodies because they may contain secrets"
        )
    )]
    HttpStatus { status: u16 },

    #[error("the OpenCode server sent an unexpected {what}")]
    #[diagnostic(
        code(hive::opencode::protocol),
        help(
            "this OpenCode version may be newer than the tested matrix; see docs/acp-compatibility.md"
        )
    )]
    Protocol { what: &'static str },
}

impl OpencodeError {
    /// The public diagnostic code, matching the `#[diagnostic(code(..))]` attribute.
    ///
    /// Spelled out as constants (rather than read back from miette) so the recovery classifier
    /// can match them without constructing an error; a unit test pins the two together.
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::Unreachable { .. } => code::UNREACHABLE,
            Self::Unauthorized => code::UNAUTHORIZED,
            Self::UnsupportedVersion { .. } => code::UNSUPPORTED_VERSION,
            Self::ApiUnavailable { .. } => code::API_UNAVAILABLE,
            Self::SessionNotFound { .. } => code::SESSION_NOT_FOUND,
            Self::AlreadyAdopted { .. } => code::ALREADY_ADOPTED,
            Self::AdoptionInProgress { .. } => code::ADOPTION_IN_PROGRESS,
            Self::ExternallyControlled => code::EXTERNALLY_CONTROLLED,
            Self::StreamLost => code::STREAM_LOST,
            Self::DirectoryMismatch { .. } => code::DIRECTORY_MISMATCH,
            Self::MissingCredentials { .. } => code::MISSING_CREDENTIALS,
            Self::UnsupportedPrompt => code::UNSUPPORTED_PROMPT,
            Self::HttpStatus { .. } => code::HTTP_STATUS,
            Self::Protocol { .. } => code::PROTOCOL,
        }
    }

    /// What to do next, from the diagnostic's help text.
    pub(crate) fn help(&self) -> String {
        Diagnostic::help(self)
            .map(|help| help.to_string())
            .unwrap_or_default()
    }

    /// Single-line form sent through worker startup and stored as a recovery reason.
    pub(crate) fn wire(&self) -> String {
        format!("{}: {}; {}", self.code(), self, self.help())
    }

    /// Structured ACP error: a stable code for programs and prose for humans.
    pub(crate) fn to_acp(&self) -> agent_client_protocol::Error {
        use agent_client_protocol::Error;
        let error = match self {
            Self::AlreadyAdopted { .. }
            | Self::AdoptionInProgress { .. }
            | Self::ExternallyControlled => Error::invalid_request(),
            Self::DirectoryMismatch { .. }
            | Self::MissingCredentials { .. }
            | Self::UnsupportedPrompt => Error::invalid_params(),
            _ => Error::internal_error(),
        };
        let mut data = serde_json::json!({
            "code": self.code(),
            "message": self.to_string(),
            "help": self.help(),
        });
        if let Self::AlreadyAdopted { hive_id, .. } = self {
            // Lets a client jump to the existing session instead of parsing prose.
            data["hiveSessionId"] = serde_json::json!(hive_id);
        }
        error.data(data)
    }

    /// Turns a bare HTTP 404 into the session-specific diagnostic.
    pub(crate) fn not_found_as(self, id: &str) -> Self {
        match self {
            Self::HttpStatus { status: 404 } => Self::SessionNotFound { id: id.into() },
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn every_variant() -> Vec<OpencodeError> {
        vec![
            OpencodeError::Unreachable { reason: "refused" },
            OpencodeError::Unauthorized,
            OpencodeError::UnsupportedVersion {
                found: "1.0.0".into(),
                minimum: "1.19.0",
            },
            OpencodeError::ApiUnavailable { what: "sessions" },
            OpencodeError::SessionNotFound { id: "ses_x".into() },
            OpencodeError::AlreadyAdopted {
                native_id: "ses_x".into(),
                hive_id: "hive-1".into(),
            },
            OpencodeError::AdoptionInProgress {
                native_id: "ses_x".into(),
            },
            OpencodeError::ExternallyControlled,
            OpencodeError::StreamLost,
            OpencodeError::DirectoryMismatch {
                native: "/a".into(),
                requested: "/b".into(),
            },
            OpencodeError::MissingCredentials { var: "VAR".into() },
            OpencodeError::UnsupportedPrompt,
            OpencodeError::HttpStatus { status: 500 },
            OpencodeError::Protocol { what: "payload" },
        ]
    }

    #[test]
    fn every_diagnostic_code_is_namespaced_and_matches_its_constant() {
        for error in every_variant() {
            let declared = Diagnostic::code(&error).map(|code| code.to_string());
            assert_eq!(declared.as_deref(), Some(error.code()), "{error:?}");
            assert!(error.code().starts_with("hive::opencode::"));
        }
    }

    #[test]
    fn every_failure_says_what_to_do_next() {
        for error in every_variant() {
            assert!(!error.help().is_empty(), "{error:?} has no help text");
            assert!(error.wire().contains(error.code()));
        }
    }

    #[test]
    fn a_missing_session_is_classified_as_lost_by_the_recovery_matcher() {
        let wire = OpencodeError::SessionNotFound { id: "ses_x".into() }.wire();
        assert!(wire.contains(code::SESSION_NOT_FOUND));
        assert!(wire.contains("saved session not found"));
    }

    #[test]
    fn a_404_becomes_a_session_not_found_but_other_statuses_are_untouched() {
        assert_eq!(
            OpencodeError::HttpStatus { status: 404 }.not_found_as("ses_x"),
            OpencodeError::SessionNotFound { id: "ses_x".into() }
        );
        assert_eq!(
            OpencodeError::HttpStatus { status: 500 }.not_found_as("ses_x"),
            OpencodeError::HttpStatus { status: 500 }
        );
    }

    #[test]
    fn an_already_adopted_error_carries_the_existing_hive_id() {
        let error = OpencodeError::AlreadyAdopted {
            native_id: "ses_x".into(),
            hive_id: "hive-9".into(),
        }
        .to_acp();
        let data = error.data.expect("structured data");
        assert_eq!(data["hiveSessionId"], "hive-9");
        assert_eq!(data["code"], code::ALREADY_ADOPTED);
    }
}
