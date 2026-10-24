//! Versioned ACP integration lives here; clients never select a downstream version.

pub use agent_client_protocol::schema::v2;
pub mod interactions;
pub use agent_client_protocol_http::{AcpHttpServer, HttpClient};

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
