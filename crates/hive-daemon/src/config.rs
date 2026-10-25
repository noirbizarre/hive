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

/// The selected path is anchored once, so reload cannot switch sources with the caller's cwd.
#[derive(Clone, Debug)]
pub struct ConfigSource {
    path: PathBuf,
    implicit: bool,
}

impl ConfigSource {
    pub fn new(path: Option<PathBuf>) -> Result<Self, ConfigError> {
        let implicit = path.is_none();
        let path = path.unwrap_or_else(default_path);
        let path = std::path::absolute(path).map_err(|_| {
            ConfigError::Unreadable(
                "cannot resolve Hive config path; check the working directory".into(),
            )
        })?;
        Ok(Self { path, implicit })
    }

    pub fn load(&self) -> Result<Config, ConfigError> {
        self.read(self.implicit)
    }

    pub fn reload(&self) -> Result<Config, ConfigError> {
        // A disappearing file must never silently erase a working registry.
        self.read(false)
    }

    fn read(&self, missing_ok: bool) -> Result<Config, ConfigError> {
        let config = Config::read_path(self.path.clone(), missing_ok)?;
        if let Some(problem) = config.problems().into_iter().next() {
            return Err(problem.error);
        }
        Ok(config)
    }
}

/// Which downstream adapter speaks for a profile.
///
/// Explicit rather than inferred from `endpoint`: an OpenCode server and an ACP
/// HTTP agent are both plain `http://` URLs, and guessing wrong would send ACP
/// frames to a REST server (or the reverse) with an unhelpful failure.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ProfileKind {
    #[default]
    Acp,
    OpencodeServer,
}

impl ProfileKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Acp => "acp",
            Self::OpencodeServer => "opencode-server",
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub name: Option<String>,
    #[serde(default)]
    pub kind: ProfileKind,
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
    /// HTTP basic-auth user for an `opencode-server` profile (OpenCode defaults to `opencode`).
    pub username: Option<String>,
    /// Name of the environment variable holding the server password. The secret itself
    /// never lives in the config file, which is routinely copied into bug reports.
    pub password_env: Option<String>,
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
    pub kind: &'static str,
    pub transport: &'a str,
    pub versions: &'a [AgentVersion],
    pub is_default: bool,
}

/// Plain HTTP is only acceptable where the traffic cannot leave the machine.
fn is_loopback_host(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Why a configuration cannot be used. Codes are public identifiers: never rename one.
///
/// The text never quotes a profile value (URL, header, env, args): config files are routinely
/// pasted into bug reports and these messages are printed by `hive doctor`.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error, miette::Diagnostic)]
pub enum ConfigError {
    #[error("{0}")]
    #[diagnostic(
        code(hive::config::unreadable),
        help("check the path and its permissions, or pass --config / set HIVE_CONFIG")
    )]
    Unreadable(String),

    #[error("{0}")]
    #[diagnostic(
        code(hive::config::syntax),
        help("fix the TOML near the reported line; unknown profile fields are rejected")
    )]
    Syntax(String),

    #[error("{0}")]
    #[diagnostic(
        code(hive::config::default_agent),
        help("set default_agent (or HIVE_DEFAULT_AGENT) to one of the ids under [agents]")
    )]
    DefaultAgent(String),

    #[error("{0}")]
    #[diagnostic(
        code(hive::config::agent_id),
        help("rename the table to [agents.<id>] using letters, digits, '-' or '_'")
    )]
    AgentId(String),

    #[error("{0}")]
    #[diagnostic(
        code(hive::config::pricing),
        help("give the profile's pricing a models.dev provider and a model or selector mapping")
    )]
    Pricing(String),

    #[error("{0}")]
    #[diagnostic(
        code(hive::config::versions),
        help("set versions = [\"v2\"], [\"v1\"] or both on the profile")
    )]
    Versions(String),

    #[error("{0}")]
    #[diagnostic(
        code(hive::config::profile),
        help(
            "give the profile either a stdio `command` (with args/env) or an `endpoint` (with headers), not both"
        )
    )]
    Profile(String),

    #[error("{0}")]
    #[diagnostic(
        code(hive::config::endpoint),
        help("use a full ws://, wss://, http:// or https:// URL without embedded credentials")
    )]
    Endpoint(String),

    #[error("{0}")]
    #[diagnostic(
        code(hive::config::header),
        help("use standard HTTP header names and printable single-line values")
    )]
    Header(String),

    #[error("{0}")]
    #[diagnostic(
        code(hive::config::credentials),
        help(
            "name an environment variable in password_env; credentials fields only apply to kind = \"opencode-server\""
        )
    )]
    Credentials(String),
}

/// One thing wrong with a configuration, attributed to a profile where there is one.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Problem {
    pub agent: Option<String>,
    pub error: ConfigError,
}

/// Header names and values are only parsed when a session connects; checking them here
/// surfaces the mistake at startup. Neither is echoed: values routinely hold tokens.
fn header_problems(id: &str, profile: &Profile, out: &mut Vec<ConfigError>) {
    for (name, value) in &profile.headers {
        if reqwest::header::HeaderName::from_bytes(name.as_bytes()).is_err() {
            out.push(ConfigError::Header(format!(
                "agent '{id}' has a header with an invalid name"
            )));
        }
        if reqwest::header::HeaderValue::from_str(value).is_err() {
            out.push(ConfigError::Header(format!(
                "agent '{id}' has a header with an invalid value"
            )));
        }
    }
}

fn validate_opencode_server(id: &str, profile: &Profile, out: &mut Vec<ConfigError>) {
    let Some(endpoint) = profile.endpoint.as_deref() else {
        out.push(ConfigError::Endpoint(format!(
            "agent '{id}' (kind = 'opencode-server') needs an http(s) endpoint, for example endpoint = 'http://127.0.0.1:4096'"
        )));
        return;
    };
    if profile.command.is_some() || !profile.args.is_empty() || !profile.env.is_empty() {
        out.push(ConfigError::Profile(format!(
            "agent '{id}' (kind = 'opencode-server') connects to a running server; remove command, args and env"
        )));
    }
    // Parsing (rather than prefix checks) is what lets us inspect scheme, host and userinfo reliably.
    match reqwest::Url::parse(endpoint) {
        Err(_) => out.push(ConfigError::Endpoint(format!(
            "agent '{id}' has an invalid endpoint; use a full URL such as http://127.0.0.1:4096"
        ))),
        Ok(url) => {
            if !matches!(url.scheme(), "http" | "https") {
                out.push(ConfigError::Endpoint(format!(
                    "agent '{id}' (kind = 'opencode-server') needs an http or https endpoint, not {}",
                    url.scheme()
                )));
            }
            if !url.username().is_empty() || url.password().is_some() {
                out.push(ConfigError::Endpoint(format!(
                    "agent '{id}' embeds credentials in its endpoint; use username and password_env instead"
                )));
            }
            if url.scheme() == "http" && !url.host_str().is_some_and(is_loopback_host) {
                out.push(ConfigError::Endpoint(format!(
                    "agent '{id}' sends OpenCode credentials over plain HTTP to a non-loopback host; use https or an SSH tunnel to a loopback address"
                )));
            }
        }
    }
    if let Some(var) = &profile.password_env
        && (var.is_empty() || !var.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
    {
        out.push(ConfigError::Credentials(format!(
            "agent '{id}' password_env must be an environment variable name, not the password itself"
        )));
    }
}

fn validate_profile(id: &str, profile: &Profile, out: &mut Vec<ConfigError>) {
    if let Some(pricing) = &profile.pricing.provider {
        if pricing.trim().is_empty()
            || (profile.pricing.model.is_none() && profile.pricing.models.is_empty())
        {
            out.push(ConfigError::Pricing(format!(
                "agent '{id}' pricing needs a provider and a fixed model or selector mapping"
            )));
        }
    } else if profile.pricing.model.is_some() || !profile.pricing.models.is_empty() {
        out.push(ConfigError::Pricing(format!(
            "agent '{id}' pricing needs a models.dev provider ID"
        )));
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
        out.push(ConfigError::Pricing(format!(
            "agent '{id}' pricing model IDs and selector values must not be empty"
        )));
    }
    if id.is_empty()
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        out.push(ConfigError::AgentId(format!(
            "invalid agent ID '{id}'; use letters, digits, '-' or '_'"
        )));
    }
    if profile.versions.is_empty() {
        out.push(ConfigError::Versions(format!(
            "agent '{id}' has no protocol versions; set versions = ['v2'] or ['v1']"
        )));
    }
    if profile.kind == ProfileKind::OpencodeServer {
        validate_opencode_server(id, profile, out);
        header_problems(id, profile, out);
        return;
    }
    if profile.username.is_some() || profile.password_env.is_some() {
        out.push(ConfigError::Credentials(format!(
            "agent '{id}' sets username or password_env, which only apply to kind = 'opencode-server'; use headers for ACP endpoints"
        )));
    }
    match (&profile.command, &profile.endpoint) {
        (Some(command), None) if !command.trim().is_empty() && profile.headers.is_empty() => {}
        (None, Some(url))
            if profile.args.is_empty()
                && profile.env.is_empty()
                && (url.starts_with("ws://")
                    || url.starts_with("wss://")
                    || url.starts_with("http://")
                    || url.starts_with("https://")) =>
        {
            // The prefix alone accepts `http://` or `ws://a b`; the URL must actually parse.
            if reqwest::Url::parse(url).is_err() {
                out.push(ConfigError::Endpoint(format!(
                    "agent '{id}' has an invalid endpoint; use a full URL such as wss://agent.example.net/acp"
                )));
            }
        }
        _ => out.push(ConfigError::Profile(format!(
            "agent '{id}' needs either a nonempty stdio command (+ args/env) or an HTTP/WebSocket endpoint (+ headers)"
        ))),
    }
    header_problems(id, profile, out);
}

impl Config {
    /// Reads and parses the file without validating it, so `hive doctor` can report every
    /// problem rather than stop at the first.
    pub fn read(path: Option<PathBuf>) -> Result<Self, ConfigError> {
        let explicit = path.is_some();
        let path = path.unwrap_or_else(default_path);
        Self::read_path(path, !explicit)
    }

    fn read_path(path: PathBuf, missing_ok: bool) -> Result<Self, ConfigError> {
        let mut config: Self = match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).map_err(|error: toml::de::Error| {
                let line = error.span().map(|span| {
                    text[..span.start]
                        .bytes()
                        .filter(|byte| *byte == b'\n')
                        .count()
                        + 1
                });
                // The toml message can quote values, so only the position is reported.
                ConfigError::Syntax(format!(
                    "invalid Hive config {}{}; check TOML syntax and profile fields",
                    path.display(),
                    line.map_or(String::new(), |line| format!(" near line {line}"))
                ))
            })?,
            Err(error) if missing_ok && error.kind() == std::io::ErrorKind::NotFound => {
                Self::default()
            }
            Err(error) => {
                return Err(ConfigError::Unreadable(format!(
                    "cannot read Hive config {}: {error}",
                    path.display()
                )));
            }
        };
        if let Ok(default) = std::env::var("HIVE_DEFAULT_AGENT") {
            config.default_agent = Some(default);
        }
        Ok(config)
    }

    pub fn load(path: Option<PathBuf>) -> Result<Self, String> {
        let config = Self::read(path).map_err(|error| error.to_string())?;
        config.validate()?;
        Ok(config)
    }

    /// Every problem, in the order `validate` would meet them.
    pub fn problems(&self) -> Vec<Problem> {
        let mut problems = Vec::new();
        if let Some(default) = &self.default_agent
            && !self.agents.contains_key(default)
        {
            problems.push(Problem {
                agent: None,
                error: ConfigError::DefaultAgent(format!(
                    "default agent '{default}' is not configured in [agents]"
                )),
            });
        }
        for (id, profile) in &self.agents {
            let mut found = Vec::new();
            validate_profile(id, profile, &mut found);
            problems.extend(found.into_iter().map(|error| Problem {
                agent: Some(id.clone()),
                error,
            }));
        }
        problems
    }

    pub fn validate(&self) -> Result<(), String> {
        match self.problems().into_iter().next() {
            Some(problem) => Err(problem.error.to_string()),
            None => Ok(()),
        }
    }

    pub fn public_profiles(&self) -> Vec<PublicProfile<'_>> {
        self.agents
            .iter()
            .map(|(id, profile)| PublicProfile {
                id,
                name: profile.name.as_deref().unwrap_or(id),
                kind: profile.kind.as_str(),
                transport: profile.transport(),
                versions: &profile.versions,
                is_default: self.default_agent.as_deref() == Some(id),
            })
            .collect()
    }
}

impl Profile {
    pub fn transport(&self) -> &'static str {
        if self.kind == ProfileKind::OpencodeServer {
            "opencode-rest+sse"
        } else if self.command.is_some() {
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
    use super::{Config, ConfigError, ConfigSource};

    #[test]
    fn a_missing_implicit_source_is_allowed_only_at_startup() {
        let source = ConfigSource {
            path: std::env::temp_dir().join(format!("hive-missing-{}.toml", uuid::Uuid::new_v4())),
            implicit: true,
        };
        assert!(source.load().is_ok());
        assert!(matches!(source.reload(), Err(ConfigError::Unreadable(_))));
        let explicit = ConfigSource {
            implicit: false,
            ..source
        };
        assert!(matches!(explicit.load(), Err(ConfigError::Unreadable(_))));
    }

    #[test]
    fn relative_sources_are_anchored_without_requiring_the_file_to_exist() {
        let path = std::path::PathBuf::from(format!("missing-{}.toml", uuid::Uuid::new_v4()));
        let source = ConfigSource::new(Some(path.clone())).unwrap();
        assert_eq!(source.path, std::env::current_dir().unwrap().join(path));
    }

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

    fn opencode(extra: &str) -> Result<(), String> {
        let config: Config =
            toml::from_str(&format!("[agents.oc]\nkind = 'opencode-server'\n{extra}\n")).unwrap();
        config.validate()
    }

    #[test]
    fn an_opencode_server_profile_is_public_without_its_endpoint_or_credentials() {
        let config: Config = toml::from_str(
            r#"
            [agents.oc]
            kind = "opencode-server"
            endpoint = "https://oc.example.net:4096"
            username = "private-user"
            password_env = "PRIVATE_PASSWORD_VAR"
        "#,
        )
        .unwrap();
        config.validate().unwrap();
        let public = serde_json::to_string(&config.public_profiles()).unwrap();
        assert!(public.contains("opencode-server"));
        assert!(public.contains("opencode-rest+sse"));
        for secret in ["oc.example.net", "private-user", "PRIVATE_PASSWORD_VAR"] {
            assert!(!public.contains(secret), "{secret} leaked");
        }
    }

    #[test]
    fn an_opencode_server_profile_requires_an_http_endpoint() {
        assert!(
            opencode("")
                .unwrap_err()
                .contains("needs an http(s) endpoint")
        );
        assert!(
            opencode("endpoint = 'ws://127.0.0.1:1'")
                .unwrap_err()
                .contains("http or https")
        );
        assert!(
            opencode("endpoint = 'http://127.0.0.1:1'\ncommand = 'opencode'")
                .unwrap_err()
                .contains("remove command")
        );
    }

    #[test]
    fn plain_http_is_refused_off_loopback_but_allowed_on_it() {
        for ok in [
            "http://127.0.0.1:4096",
            "http://localhost:4096",
            "http://[::1]:4096",
            "https://oc.example.net",
        ] {
            opencode(&format!("endpoint = '{ok}'")).unwrap_or_else(|e| panic!("{ok}: {e}"));
        }
        let error = opencode("endpoint = 'http://oc.example.net:4096'").unwrap_err();
        assert!(error.contains("plain HTTP"), "{error}");
    }

    #[test]
    fn credentials_embedded_in_the_endpoint_are_rejected() {
        let error = opencode("endpoint = 'https://user:pw@oc.example.net'").unwrap_err();
        assert!(error.contains("username and password_env"), "{error}");
        assert!(!error.contains("pw@"), "the error must not echo the secret");
    }

    #[test]
    fn password_env_must_name_a_variable_rather_than_hold_a_password() {
        let error =
            opencode("endpoint = 'http://127.0.0.1:1'\npassword_env = 'p@ss word'").unwrap_err();
        assert!(error.contains("environment variable name"), "{error}");
        assert!(
            !error.contains("p@ss"),
            "the error must not echo the secret"
        );
    }

    #[test]
    fn credentials_fields_are_rejected_on_acp_profiles() {
        let config: Config =
            toml::from_str("[agents.a]\nendpoint = 'http://127.0.0.1:1'\npassword_env = 'X'\n")
                .unwrap();
        assert!(
            config
                .validate()
                .unwrap_err()
                .contains("only apply to kind")
        );
    }

    #[test]
    fn every_problem_is_reported_rather_than_only_the_first() {
        let config: Config = toml::from_str(
            r#"default_agent = "missing"
            [agents.a]
            [agents."bad id"]
            command = "x"
            versions = []
        "#,
        )
        .unwrap();
        let codes: Vec<_> = config
            .problems()
            .iter()
            .map(|p| miette::Diagnostic::code(&p.error).unwrap().to_string())
            .collect();
        assert_eq!(
            codes,
            [
                "hive::config::default_agent",
                "hive::config::profile",
                "hive::config::agent_id",
                "hive::config::versions",
            ]
        );
    }

    #[test]
    fn validate_reports_the_same_first_message_as_before() {
        let config: Config =
            toml::from_str("default_agent = 'missing'\n[agents.a]\ncommand = 'x'").unwrap();
        assert_eq!(
            config.validate().unwrap_err(),
            "default agent 'missing' is not configured in [agents]"
        );
    }

    #[test]
    fn invalid_headers_and_endpoints_are_rejected_without_echoing_them() {
        let config: Config = toml::from_str(
            r#"[agents.badheader]
            endpoint = "https://agent.example.net"
            headers = { "X-Token" = "secret-value\nsecond-line" }
            [agents.badurl]
            endpoint = "http://secret host"
        "#,
        )
        .unwrap();
        let problems = config.problems();
        let codes: Vec<_> = problems
            .iter()
            .map(|p| miette::Diagnostic::code(&p.error).unwrap().to_string())
            .collect();
        assert_eq!(codes, ["hive::config::header", "hive::config::endpoint"]);
        let text = format!("{problems:?}");
        for secret in ["secret-value", "secret host", "X-Token"] {
            assert!(!text.contains(secret), "{secret} leaked");
        }
    }

    #[test]
    fn config_diagnostic_codes_are_stable_public_identifiers() {
        use miette::Diagnostic;
        let errors = [
            super::ConfigError::Unreadable(String::new()),
            super::ConfigError::Syntax(String::new()),
            super::ConfigError::DefaultAgent(String::new()),
            super::ConfigError::AgentId(String::new()),
            super::ConfigError::Pricing(String::new()),
            super::ConfigError::Versions(String::new()),
            super::ConfigError::Profile(String::new()),
            super::ConfigError::Endpoint(String::new()),
            super::ConfigError::Header(String::new()),
            super::ConfigError::Credentials(String::new()),
        ];
        let codes: Vec<_> = errors
            .iter()
            .map(|e| e.code().unwrap().to_string())
            .collect();
        assert_eq!(
            codes,
            [
                "hive::config::unreadable",
                "hive::config::syntax",
                "hive::config::default_agent",
                "hive::config::agent_id",
                "hive::config::pricing",
                "hive::config::versions",
                "hive::config::profile",
                "hive::config::endpoint",
                "hive::config::header",
                "hive::config::credentials",
            ]
        );
        assert!(errors.iter().all(|e| e.help().is_some()));
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
