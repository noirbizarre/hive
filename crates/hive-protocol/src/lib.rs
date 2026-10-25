//! Versioned ACP integration lives here; clients never select a downstream version.

pub use agent_client_protocol::schema::v2;
pub mod interactions;
pub use agent_client_protocol_http::{AcpHttpServer, HttpClient};

/// Controlled diagnostics must never echo arbitrary provider payloads into retained session history.
#[derive(Clone, Copy, Debug, thiserror::Error)]
pub enum AgentFailure {
    #[error("The provider API key has expired")]
    ExpiredKey,
    #[error("The downstream agent failed the prompt")]
    Prompt,
}

impl AgentFailure {
    pub fn classify(details: &str) -> Self {
        // Recognize an actionable known failure without forwarding any surrounding request data.
        if details.contains("This API key has expired.") {
            Self::ExpiredKey
        } else {
            Self::Prompt
        }
    }

    pub fn code(self) -> &'static str {
        match self {
            Self::ExpiredKey => "hive::agent::expired_key",
            Self::Prompt => "hive::agent::prompt_failed",
        }
    }

    pub fn help(self) -> &'static str {
        match self {
            Self::ExpiredKey => {
                "Create a new API key in OpenCode Console and replace it in your client or secret store; signing in again does not renew the key"
            }
            Self::Prompt => {
                "Check the agent's provider credentials and model configuration, and its logs; then send a new prompt"
            }
        }
    }

    pub fn update(self, source: &str) -> v2::SessionUpdate {
        v2::SessionUpdate::Other(v2::OtherSessionUpdate::new(
            "_hive_error",
            std::collections::BTreeMap::from([
                ("version".into(), serde_json::json!(1)),
                ("source".into(), serde_json::json!(source)),
                ("operation".into(), serde_json::json!("session/prompt")),
                ("code".into(), serde_json::json!(self.code())),
                ("message".into(), serde_json::json!(self.to_string())),
                ("help".into(), serde_json::json!(self.help())),
            ]),
        ))
    }
}

/// All native clients use the same loopback status route; ACP methods stay on /acp.
pub async fn daemon_status(
    base_url: &str,
) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
    let url = format!("{}/v1/status", base_url.trim_end_matches('/'));
    Ok(reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()?
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentVersion {
    V1,
    V2,
}
