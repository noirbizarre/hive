//! Profile declarations are inert until a client starts a session.

use std::{collections::BTreeMap, path::PathBuf};

use hive_protocol::AgentVersion;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub default_agent: Option<String>,
    #[serde(default)]
    pub agents: BTreeMap<String, Profile>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub name: Option<String>,
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub endpoint: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default = "v2_only")]
    pub versions: Vec<AgentVersion>,
    #[serde(default)]
    pub pricing: Pricing,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pricing {
    /// Exact models.dev provider ID, never inferred from the Hive agent profile.
    pub provider: Option<String>,
    /// Fixed model ID when this agent cannot switch models during a session.
    pub model: Option<String>,
    /// ACP model-selector value to models.dev model ID mapping.
    #[serde(default)]
    pub models: BTreeMap<String, String>,
    /// Opt in only when the agent's ACP inputTokens includes both cache categories.
    #[serde(default)]
    pub cached_input_included: bool,
}

fn v2_only() -> Vec<AgentVersion> {
    vec![AgentVersion::V2]
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicProfile<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub transport: &'a str,
    pub versions: &'a [AgentVersion],
    pub is_default: bool,
}

impl Config {
    pub fn load(path: Option<PathBuf>) -> Result<Self, String> {
        let explicit = path.is_some();
        let path = path.unwrap_or_else(default_path);
        let mut config: Self = match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).map_err(|error: toml::de::Error| {
                let line = error.span().map(|span| {
                    text[..span.start]
                        .bytes()
                        .filter(|byte| *byte == b'\n')
                        .count()
                        + 1
                });
                format!(
                    "invalid Hive config {}{}; check TOML syntax and profile fields",
                    path.display(),
                    line.map_or(String::new(), |line| format!(" near line {line}"))
                )
            })?,
            Err(error) if !explicit && error.kind() == std::io::ErrorKind::NotFound => {
                Self::default()
            }
            Err(error) => {
                return Err(format!(
                    "cannot read Hive config {}: {error}",
                    path.display()
                ));
            }
        };
        if let Ok(default) = std::env::var("HIVE_DEFAULT_AGENT") {
            config.default_agent = Some(default);
        }
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), String> {
        if let Some(default) = &self.default_agent
            && !self.agents.contains_key(default)
        {
            return Err(format!(
                "default agent '{default}' is not configured in [agents]"
            ));
        }
        for (id, profile) in &self.agents {
            if let Some(pricing) = &profile.pricing.provider {
                if pricing.trim().is_empty()
                    || (profile.pricing.model.is_none() && profile.pricing.models.is_empty())
                {
                    return Err(format!(
                        "agent '{id}' pricing needs a provider and a fixed model or selector mapping"
                    ));
                }
            } else if profile.pricing.model.is_some() || !profile.pricing.models.is_empty() {
                return Err(format!(
                    "agent '{id}' pricing needs a models.dev provider ID"
                ));
            }
            if profile
                .pricing
                .model
                .as_ref()
                .is_some_and(|model| model.trim().is_empty())
                || profile
                    .pricing
                    .models
                    .iter()
                    .any(|(selector, model)| selector.trim().is_empty() || model.trim().is_empty())
            {
                return Err(format!(
                    "agent '{id}' pricing model IDs and selector values must not be empty"
                ));
            }
            if id.is_empty()
                || !id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                return Err(format!(
                    "invalid agent ID '{id}'; use letters, digits, '-' or '_'"
                ));
            }
            if profile.versions.is_empty() {
                return Err(format!(
                    "agent '{id}' has no protocol versions; set versions = ['v2'] or ['v1']"
                ));
            }
            match (&profile.command, &profile.endpoint) {
                (Some(command), None)
                    if !command.trim().is_empty() && profile.headers.is_empty() => {}
                (None, Some(url))
                    if profile.args.is_empty()
                        && profile.env.is_empty()
                        && (url.starts_with("ws://")
                            || url.starts_with("wss://")
                            || url.starts_with("http://")
                            || url.starts_with("https://")) => {}
                _ => {
                    return Err(format!(
                        "agent '{id}' needs either a nonempty stdio command (+ args/env) or an HTTP/WebSocket endpoint (+ headers)"
                    ));
                }
            }
        }
        Ok(())
    }

    pub fn public_profiles(&self) -> Vec<PublicProfile<'_>> {
        self.agents
            .iter()
            .map(|(id, profile)| PublicProfile {
                id,
                name: profile.name.as_deref().unwrap_or(id),
                transport: profile.transport(),
                versions: &profile.versions,
                is_default: self.default_agent.as_deref() == Some(id),
            })
            .collect()
    }
}

impl Profile {
    pub fn transport(&self) -> &'static str {
        if self.command.is_some() {
            "stdio"
        } else if self
            .endpoint
            .as_deref()
            .is_some_and(|url| url.starts_with("ws"))
        {
            "websocket"
        } else {
            "http+sse"
        }
    }

    pub fn preferred_version(&self) -> AgentVersion {
        if self.versions.contains(&AgentVersion::V2) {
            AgentVersion::V2
        } else {
            AgentVersion::V1
        }
    }
}

pub fn default_path() -> PathBuf {
    let root = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    root.join("hive/config.toml")
}

#[cfg(test)]
mod tests {
    use super::Config;

    #[test]
    fn multiple_profiles_parse_without_disclosing_private_launch_settings() {
        let config: Config = toml::from_str(
            r#"
            default_agent = "pi"
            [agents.pi]
            name = "Pi"
            command = "/secret/bin/pi-acp"
            args = ["--private-flag"]
            versions = ["v1"]
            env = { API_KEY = "private-token" }
            [agents.opencode]
            endpoint = "http://127.0.0.1:9000"
            versions = ["v1", "v2"]
            headers = { Authorization = "Bearer private-token" }
        "#,
        )
        .unwrap();
        config.validate().unwrap();
        let public = serde_json::to_string(&config.public_profiles()).unwrap();
        assert!(public.contains("opencode"));
        for secret in [
            "private-token",
            "/secret/bin/pi-acp",
            "--private-flag",
            "Authorization",
            "127.0.0.1:9000",
        ] {
            assert!(!public.contains(secret));
        }
    }

    #[test]
    fn a_bad_default_is_rejected_before_the_listener_starts() {
        let config: Config = toml::from_str("default_agent = 'missing'").unwrap();
        assert!(config.validate().unwrap_err().contains("not configured"));
    }

    #[test]
    fn pricing_requires_explicit_catalog_identity_and_keeps_it_out_of_public_profiles() {
        let missing: Config = toml::from_str(
            r#"[agents.worker]
            command = "mock"
            [agents.worker.pricing]
            model = "example"
        "#,
        )
        .unwrap();
        assert!(missing.validate().unwrap_err().contains("provider ID"));
        let configured: Config = toml::from_str(
            r#"[agents.worker]
            command = "mock"
            [agents.worker.pricing]
            provider = "anthropic"
            models = { "agent-alias" = "claude-example" }
        "#,
        )
        .unwrap();
        configured.validate().unwrap();
        assert_eq!(
            configured.agents["worker"].pricing.models["agent-alias"],
            "claude-example"
        );
        assert!(
            !serde_json::to_string(&configured.public_profiles())
                .unwrap()
                .contains("claude-example")
        );
    }
}
