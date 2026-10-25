//! History authority and coverage are independent of execution/recovery readiness.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Owner {
    #[default]
    Agent,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    #[default]
    Unknown,
    Live,
    AgentHistory,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Restoration {
    #[default]
    Unknown,
    Supported,
    Unsupported,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Availability {
    Loading,
    Available,
    #[default]
    Unavailable,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Completeness {
    // ACP promises retained history, not a certificate of indefinite retention.
    #[default]
    Unknown,
    Partial,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WarningCode {
    #[serde(rename = "hive::transcript::unsupported")]
    Unsupported,
    #[serde(rename = "hive::transcript::history_missing")]
    HistoryMissing,
    #[serde(rename = "hive::transcript::truncated")]
    Truncated,
    #[serde(rename = "hive::transcript::state_missing")]
    StateMissing,
    #[serde(rename = "hive::transcript::projection_limited")]
    ProjectionLimited,
    #[serde(rename = "hive::transcript::operation_rejected")]
    OperationRejected,
}

impl WarningCode {
    pub fn message(self) -> &'static str {
        match self {
            Self::Unsupported => {
                "The agent does not advertise history restoration; keep Hive running to retain this view, or use a replay-capable agent"
            }
            Self::HistoryMissing => {
                "Earlier work was recorded but the agent returned no conversation messages; check its history retention"
            }
            Self::Truncated => {
                "Hive imported a bounded portion of retained history; consult the agent for earlier content"
            }
            Self::StateMissing => {
                "The agent resumed without the required current-state update; verify its ACP implementation before sending work"
            }
            Self::ProjectionLimited => {
                "Some history entities cannot be displayed by Hive; consult the agent for the original history"
            }
            Self::OperationRejected => {
                "The agent rejected an advertised history operation; check its protocol implementation and retained sessions"
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Warning {
    pub code: WarningCode,
    pub message: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TranscriptStatus {
    pub owner: Owner,
    pub source: Source,
    pub restoration: Restoration,
    pub availability: Availability,
    pub completeness: Completeness,
    pub warnings: Vec<Warning>,
}

impl TranscriptStatus {
    pub fn warn(&mut self, code: WarningCode) {
        // Repeated reconnects must not accumulate duplicate warnings in clients.
        if !self.warnings.iter().any(|warning| warning.code == code) {
            self.warnings.push(Warning {
                code,
                message: code.message().into(),
            });
        }
    }

    pub fn partial(&mut self, code: WarningCode) {
        self.completeness = Completeness::Partial;
        self.warn(code);
    }

    pub fn label(&self) -> String {
        let coverage = match self.completeness {
            Completeness::Unknown => "History: retention coverage unknown",
            Completeness::Partial => "History: partial",
        };
        std::iter::once(coverage)
            .chain(self.warnings.iter().map(|warning| warning.message.as_str()))
            .collect::<Vec<_>>()
            .join(" · ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_fields_do_not_claim_replay_support_or_available_history() {
        let status: TranscriptStatus = serde_json::from_str("{}").unwrap();
        assert_eq!(status, TranscriptStatus::default());
        assert_eq!(serde_json::to_value(status).unwrap()["owner"], "agent");
    }

    #[test]
    fn repeated_warnings_preserve_partial_coverage_without_duplicates() {
        let mut status = TranscriptStatus::default();
        status.partial(WarningCode::Truncated);
        status.warn(WarningCode::Truncated);
        assert_eq!(status.warnings.len(), 1);
        assert_eq!(status.completeness, Completeness::Partial);
        let wire = serde_json::to_value(&status).unwrap();
        assert_eq!(wire["warnings"][0]["code"], "hive::transcript::truncated");
        assert_eq!(
            serde_json::from_value::<TranscriptStatus>(wire).unwrap(),
            status
        );
    }
}
