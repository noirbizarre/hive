//! Portable declarations are resolved once at the trusted configuration boundary.

use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
};

use serde::Deserialize;

use super::*;

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpConfig {
    #[serde(default)]
    pub servers: BTreeMap<String, ConfiguredServer>,
}

pub(crate) fn validate_id(id: &str) -> Result<(), &'static str> {
    if id.is_empty()
        || id.len() > 128
        || id == "hive-managed-mcp"
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(
            "MCP IDs must use 1-128 letters, digits, '.', '-' or '_' and cannot be hive-managed-mcp",
        );
    }
    Ok(())
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfiguredServer {
    command: Option<Vec<String>>,
    url: Option<String>,
    cwd: Option<PathBuf>,
    // Options distinguish an absent table from an invalid cross-transport empty table.
    env: Option<BTreeMap<String, ConfigValue>>,
    headers: Option<BTreeMap<String, ConfigValue>>,
    hive: Option<HiveOptions>,
}

#[derive(Clone, Deserialize)]
#[serde(untagged)]
enum ConfigValue {
    Literal(String),
    Reference(EnvironmentReference),
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct EnvironmentReference {
    env: String,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HiveOptions {
    #[serde(default = "workspace_scope")]
    scope: Scope,
    #[serde(default = "idle_timeout")]
    idle_timeout_seconds: u64,
    sharing: Option<SharedPolicy>,
}

#[derive(Clone, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum SharedPolicy {
    Shared {
        principal: String,
        partition: String,
        policy_generation: String,
        external_generations: BTreeMap<String, String>,
    },
}

fn workspace_scope() -> Scope {
    Scope::Workspace
}
fn idle_timeout() -> u64 {
    60
}

impl Default for HiveOptions {
    fn default() -> Self {
        Self {
            scope: workspace_scope(),
            idle_timeout_seconds: idle_timeout(),
            sharing: None,
        }
    }
}

/// Never serialize prepared inputs: configured secrets must not enter durable records.
#[derive(Clone)]
pub(crate) enum PreparedServer {
    Stdio {
        command: Vec<String>,
        env: BTreeMap<String, String>,
        effective_env: BTreeMap<OsString, OsString>,
        lookup_env: BTreeMap<OsString, OsString>,
        cwd: Option<PathBuf>,
        hive: HiveOptions,
    },
    Http {
        url: String,
        headers: BTreeMap<String, String>,
    },
}

impl fmt::Debug for ConfiguredServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ConfiguredServer([redacted])")
    }
}
impl fmt::Debug for PreparedServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PreparedServer([redacted])")
    }
}

pub(crate) type Environment = BTreeMap<OsString, OsString>;

pub(crate) fn environment() -> Environment {
    std::env::vars_os().collect()
}

fn resolve_values(
    values: &Option<BTreeMap<String, ConfigValue>>,
    environment: &Environment,
    field: &str,
) -> Result<BTreeMap<String, String>, String> {
    values.iter().flatten().map(|(name, value)| {
        let value = match value {
            ConfigValue::Literal(value) => value.clone(),
            ConfigValue::Reference(reference) => {
                if !valid_env_name(&reference.env) {
                    return Err(format!("{field}: environment reference must name a nonempty variable without '=' or NUL"));
                }
                // References use the daemon snapshot, never entries in the child overlay.
                environment_value(environment, &reference.env)
                    .and_then(|value| value.to_str())
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| format!("{field}.{name}: referenced environment variable '{}' is missing, empty or not Unicode; export a nonempty value in the daemon environment", reference.env))?
                    .to_owned()
            }
        };
        Ok((name.clone(), value))
    }).collect()
}

fn environment_name_matches(key: &OsStr, name: &str) -> bool {
    if cfg!(windows) {
        key.to_string_lossy().eq_ignore_ascii_case(name)
    } else {
        key == OsStr::new(name)
    }
}

fn environment_value<'a>(environment: &'a Environment, name: &str) -> Option<&'a OsString> {
    environment
        .iter()
        .find(|(key, _)| environment_name_matches(key, name))
        .map(|(_, value)| value)
}

impl ConfiguredServer {
    pub fn managed(&self) -> bool {
        self.command.is_some()
    }

    pub(crate) fn prepare(&self, environment: &Environment) -> Result<PreparedServer, String> {
        match (&self.command, &self.url) {
            (Some(command), None) => {
                if self.headers.is_some() {
                    return Err("stdio cannot declare headers; remove the headers table".into());
                }
                if command.is_empty()
                    || !nonblank(&command[0])
                    || command.iter().any(|arg| arg.contains('\0'))
                {
                    return Err("command must be a nonempty array with a nonblank executable and NUL-free literal arguments".into());
                }
                let hive = self.hive.clone().unwrap_or_default();
                if hive.scope != Scope::Workspace
                    && (self.cwd.as_ref().is_none_or(|cwd| !cwd.is_absolute())
                        || (Path::new(&command[0]).components().count() > 1
                            && !Path::new(&command[0]).is_absolute()))
                {
                    return Err("machine/project scope requires an explicit absolute cwd and an absolute or bare PATH executable".into());
                }
                if self.cwd.as_ref().is_some_and(|cwd| {
                    cwd.as_os_str().is_empty() || cwd.to_string_lossy().contains('\0')
                }) {
                    return Err("cwd must be a nonempty NUL-free path".into());
                }
                if let Some(SharedPolicy::Shared {
                    principal,
                    partition,
                    policy_generation,
                    external_generations,
                }) = &hive.sharing
                    && (!nonblank(principal)
                        || !nonblank(partition)
                        || !nonblank(policy_generation)
                        || external_generations
                            .iter()
                            .any(|(key, value)| !nonblank(key) || !nonblank(value)))
                {
                    return Err("shared MCP requires nonblank principal, partition, policy and credential generations".into());
                }
                let env = resolve_values(&self.env, environment, "env")?;
                // Match Bee's baseline so unrelated daemon/provider credentials cannot escape.
                let baseline = [
                    "PATH",
                    "HOME",
                    "USERPROFILE",
                    "TMPDIR",
                    "TMP",
                    "TEMP",
                    "SystemRoot",
                    "WINDIR",
                    "COMSPEC",
                    "PATHEXT",
                ];
                let mut effective_env: Environment = environment
                    .iter()
                    .filter(|(key, _)| {
                        baseline
                            .iter()
                            .any(|name| environment_name_matches(key, name))
                    })
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect();
                for (key, value) in &env {
                    // Windows environment names are case-insensitive; explicit overrides win once.
                    if cfg!(windows) {
                        effective_env
                            .retain(|old, _| !old.to_string_lossy().eq_ignore_ascii_case(key));
                    }
                    effective_env.insert(key.into(), value.into());
                }
                let prepared = PreparedServer::Stdio {
                    command: command.clone(),
                    env,
                    effective_env,
                    // A child PATH override must not redirect the initial trusted executable lookup.
                    lookup_env: environment
                        .iter()
                        .filter(|(key, _)| {
                            environment_name_matches(key, "PATH")
                                || environment_name_matches(key, "PATHEXT")
                        })
                        .map(|(key, value)| (key.clone(), value.clone()))
                        .collect(),
                    cwd: self.cwd.clone(),
                    hive,
                };
                prepared.definition().map_err(|_| {
                    "invalid stdio environment; use valid variable names and NUL-free values"
                        .to_owned()
                })?;
                Ok(prepared)
            }
            (None, Some(url)) => {
                if self.cwd.is_some() || self.env.is_some() || self.hive.is_some() {
                    return Err("HTTP cannot declare cwd, env or Hive process controls; remove those fields".into());
                }
                let headers = resolve_values(&self.headers, environment, "headers")?;
                validate_http(url, &headers).map_err(|_| "invalid HTTP endpoint or headers; use a credential-free HTTP(S) URL and unique valid headers".to_owned())?;
                let parsed = reqwest::Url::parse(url).map_err(|_| "invalid HTTP URL".to_owned())?;
                let loopback = parsed.host_str().is_some_and(|host| {
                    host.eq_ignore_ascii_case("localhost")
                        || host
                            .trim_matches(['[', ']'])
                            .parse::<std::net::IpAddr>()
                            .is_ok_and(|ip| ip.is_loopback())
                });
                if parsed.scheme() != "https" && !loopback {
                    return Err("HTTP endpoints require HTTPS except on localhost or a loopback IP; use an HTTPS URL".into());
                }
                let reserved = [
                    "host",
                    "accept",
                    "content-type",
                    "content-length",
                    "connection",
                    "transfer-encoding",
                    "mcp-session-id",
                    "mcp-protocol-version",
                    "last-event-id",
                ];
                if headers
                    .keys()
                    .any(|name| reserved.contains(&name.to_ascii_lowercase().as_str()))
                {
                    return Err("headers cannot override MCP protocol or HTTP routing headers; remove the reserved header".into());
                }
                Ok(PreparedServer::Http {
                    url: url.clone(),
                    headers,
                })
            }
            _ => Err(
                "declare exactly one of command = [executable, arg, ...] or url = 'https://...'"
                    .into(),
            ),
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        self.prepare(&environment()).map(|_| ())
    }
}

impl PreparedServer {
    pub(crate) fn managed(&self) -> bool {
        matches!(self, Self::Stdio { .. })
    }

    pub(crate) fn definition(&self) -> Result<ServerDefinition, McpError> {
        match self {
            Self::Stdio {
                command, env, hive, ..
            } => ServerDefinition::new(
                Transport::Stdio {
                    command: command[0].clone(),
                    args: command[1..].to_vec(),
                    env: env.clone(),
                },
                hive.scope,
                Ownership::Managed,
                Lifecycle::OnDemand {
                    idle_timeout_seconds: hive.idle_timeout_seconds,
                },
            ),
            Self::Http { url, headers } => ServerDefinition::new(
                Transport::Http {
                    url: url.clone(),
                    headers: headers.clone(),
                },
                Scope::Machine,
                Ownership::Forwarded,
                Lifecycle::Delegated,
            ),
        }
    }

    pub(crate) fn scope(&self) -> Scope {
        match self {
            Self::Stdio { hive, .. } => hive.scope,
            Self::Http { .. } => Scope::Machine,
        }
    }

    pub(crate) fn admission(
        &self,
        id: &str,
        node: hive_core::identity::NodeId,
        root: Option<&Path>,
    ) -> Result<StdioAdmission, AdmissionError> {
        let Self::Stdio {
            command,
            effective_env,
            lookup_env,
            cwd,
            hive,
            ..
        } = self
        else {
            return Err(runtime::RuntimeError::Inputs.into());
        };
        let syntax = if cfg!(windows) {
            hive_core::workspace::PathSyntax::Windows
        } else {
            hive_core::workspace::PathSyntax::Posix
        };
        let path = |p: &Path| {
            hive_core::workspace::NodePath::new(
                node.clone(),
                syntax,
                p.to_str().ok_or(AdmissionError::Path)?.into(),
            )
            .map_err(|_| AdmissionError::Path)
        };
        let anchor = |p: &Path| -> Result<PathBuf, AdmissionError> {
            if p.is_absolute() {
                Ok(p.into())
            } else {
                // Only the workspace admission gate can supply this authoritative root.
                Ok(root.ok_or(AdmissionError::Scope)?.join(p))
            }
        };
        let executable = Path::new(&command[0]);
        let executable =
            if executable.components().count() == 1 && !command[0].contains(['/', '\\']) {
                lookup_executable(executable, lookup_env, root)?
            } else {
                anchor(executable)?
            };
        let cwd = match cwd {
            Some(cwd) => anchor(cwd)?,
            None => root.ok_or(AdmissionError::Scope)?.into(),
        };
        if !executable_file(&executable) {
            return Err(AdmissionError::Path);
        }
        let (security, sharing) = match &hive.sharing {
            Some(SharedPolicy::Shared {
                principal,
                partition,
                policy_generation,
                external_generations,
            }) => (
                SecurityContext {
                    principal: principal.clone(),
                    partition: partition.clone(),
                    policy_generation: policy_generation.clone(),
                    credentials: Credentials::Complete {
                        external_generations: external_generations.clone(),
                    },
                },
                Sharing::Shared,
            ),
            None => (
                SecurityContext {
                    principal: "hive-daemon-owner".into(),
                    partition: "private".into(),
                    policy_generation: "private-v1".into(),
                    credentials: Credentials::Unknown,
                },
                Sharing::Private,
            ),
        };
        Ok(StdioAdmission {
            definition_id: id.into(),
            definition: self.definition()?,
            launch: StdioLaunch {
                executable: path(&executable)?,
                cwd: Some(path(&cwd)?),
                env: effective_env.clone(),
            },
            initialization: Initialization {
                protocol_version: "2025-11-25".into(),
                client_info: serde_json::json!({"name":"hive", "version":env!("CARGO_PKG_VERSION")}),
                capabilities: serde_json::json!({}),
                options: serde_json::json!({}),
                roots: vec![],
            },
            security,
            sharing,
        })
    }
}

fn lookup_executable(
    command: &Path,
    environment: &Environment,
    root: Option<&Path>,
) -> Result<PathBuf, AdmissionError> {
    let paths = environment_value(environment, "PATH").ok_or(AdmissionError::Path)?;
    for directory in std::env::split_paths(paths) {
        let directory = if directory.is_absolute() {
            directory
        } else if let Some(root) = root {
            root.join(directory)
        } else {
            // Never fall back to the daemon's ambient cwd for relative PATH entries.
            continue;
        };
        let candidate = directory.join(command);
        let mut candidates = vec![candidate.clone()];
        if cfg!(windows) && command.extension().is_none() {
            candidates.extend(
                environment_value(environment, "PATHEXT")
                    .and_then(|value| value.to_str())
                    .unwrap_or(".COM;.EXE;.BAT;.CMD")
                    .split(';')
                    .filter(|ext| !ext.is_empty())
                    .map(|ext| candidate.with_extension(ext.trim_start_matches('.'))),
            );
        }
        for candidate in candidates {
            if executable_file(&candidate) {
                return Ok(candidate);
            }
        }
    }
    Err(AdmissionError::Path)
}

fn executable_file(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn declaration(value: serde_json::Value) -> ConfiguredServer {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn portable_commands_default_to_private_workspace_instances_with_only_baseline_environment() {
        let environment = Environment::from([
            ("PATH".into(), "/bin".into()),
            ("HOME".into(), "/home/example".into()),
            ("PROVIDER_TOKEN".into(), "hidden-secret".into()),
            ("SERVICE_TOKEN".into(), "service-secret".into()),
        ]);
        let raw = declaration(
            json!({"command":["server", "literal $HOME"],"cwd":".","env":{"TOKEN":{"env":"SERVICE_TOKEN"},"MODE":"readonly","HOME":"/explicit"}}),
        );
        let prepared = raw.prepare(&environment).unwrap();
        let PreparedServer::Stdio {
            command,
            env,
            effective_env,
            hive,
            ..
        } = &prepared
        else {
            panic!("expected stdio")
        };
        assert_eq!(command, &["server", "literal $HOME"]);
        assert_eq!(hive.scope, Scope::Workspace);
        assert_eq!(hive.idle_timeout_seconds, 60);
        assert!(hive.sharing.is_none());
        assert_eq!(env["TOKEN"], "service-secret");
        assert_eq!(effective_env[OsStr::new("HOME")], "/explicit");
        assert!(!effective_env.contains_key(OsStr::new("PROVIDER_TOKEN")));
        assert!(!effective_env.contains_key(OsStr::new("SERVICE_TOKEN")));
        assert_eq!(effective_env[OsStr::new("TOKEN")], "service-secret");
        assert!(!format!("{raw:?} {prepared:?}").contains("secret"));
    }

    #[test]
    fn references_are_whole_values_from_the_daemon_and_freeze_before_admission() {
        let raw = declaration(
            json!({"url":"https://example.test/mcp","headers":{"Authorization":{"env":"AUTH"},"x-literal":"${AUTH}"}}),
        );
        let mut environment = Environment::from([("AUTH".into(), "Bearer first-secret".into())]);
        let first = raw.prepare(&environment).unwrap();
        environment.insert("AUTH".into(), "Bearer second-secret".into());
        let second = raw.prepare(&environment).unwrap();
        let PreparedServer::Http { headers, .. } = first else {
            panic!("expected HTTP")
        };
        assert_eq!(headers["Authorization"], "Bearer first-secret");
        assert_eq!(headers["x-literal"], "${AUTH}");
        assert_ne!(
            second.definition().unwrap(),
            raw.prepare(&Environment::from([("AUTH".into(), "different".into())]))
                .unwrap()
                .definition()
                .unwrap()
        );
        for environment in [
            Environment::new(),
            Environment::from([("AUTH".into(), "".into())]),
        ] {
            let error = raw.prepare(&environment).unwrap_err();
            assert!(error.contains("AUTH"));
            assert!(error.contains("headers"));
            assert!(!error.contains("secret"));
        }
    }

    #[test]
    fn rotating_a_referenced_secret_splits_otherwise_identical_shared_instances() {
        let raw = declaration(json!({
            "command":[std::env::current_exe().unwrap()], "cwd":std::env::current_dir().unwrap(),
            "env":{"TOKEN":{"env":"SERVICE_TOKEN"}},
            "hive":{"scope":"machine", "sharing":{"mode":"shared", "principal":"owner", "partition":"team", "policy_generation":"1", "external_generations":{}}}
        }));
        let node = hive_core::identity::NodeId::new();
        let identity = |secret: &str| {
            raw.prepare(&Environment::from([(
                "SERVICE_TOKEN".into(),
                secret.into(),
            )]))
            .unwrap()
            .admission("files", node.clone(), None)
            .unwrap()
            .identity(
                ScopeContext {
                    node: node.clone(),
                    project: None,
                    workspace: None,
                },
                None,
            )
            .unwrap()
        };
        assert_eq!(identity("first-secret"), identity("first-secret"));
        assert_ne!(identity("first-secret"), identity("rotated-secret"));
        assert!(!format!("{:?}", identity("first-secret")).contains("first-secret"));
    }

    #[test]
    fn invalid_transport_combinations_and_protocol_overrides_fail_before_forwarding() {
        for value in [
            json!({}),
            json!({"command":[]}),
            json!({"command":[""]}),
            json!({"command":["server"],"url":"https://example.test"}),
            json!({"command":["server"],"headers":{}}),
            json!({"url":"https://example.test","env":{}}),
            json!({"url":"https://example.test","cwd":"."}),
            json!({"url":"https://example.test","hive":{}}),
            json!({"url":"http://example.test/mcp"}),
            json!({"url":"https://user:secret@example.test/mcp"}),
            json!({"url":"https://example.test/mcp#secret"}),
            json!({"url":"https://example.test","headers":{"Authorization":"a","authorization":"b"}}),
            json!({"url":"https://example.test","headers":{"Mcp-Session-Id":"override"}}),
            json!({"url":"https://example.test","headers":{"Host":"override"}}),
            json!({"command":["server"],"hive":{"scope":"machine"}}),
            json!({"command":["./server"],"cwd":"/tmp","hive":{"scope":"project"}}),
        ] {
            assert!(
                declaration(value.clone())
                    .prepare(&Environment::new())
                    .is_err(),
                "accepted {value}"
            );
        }
        for url in [
            "http://localhost/mcp",
            "http://127.0.0.1/mcp",
            "http://[::1]/mcp",
            "https://example.test/mcp",
        ] {
            declaration(json!({"url":url}))
                .prepare(&Environment::new())
                .unwrap();
        }
    }

    #[test]
    fn unknown_portable_and_extension_fields_and_malformed_references_are_rejected() {
        for value in [
            json!({"command":["server"],"transport":"stdio"}),
            json!({"command":["server"],"args":[]}),
            json!({"command":"server"}),
            json!({"command":["server"],"scope":"machine"}),
            json!({"command":["server"],"hive":{"unknown":true}}),
            json!({"command":["server"],"env":{"TOKEN":{"env":"TOKEN","extra":"secret"}}}),
            json!({"url":"https://example.test","headers":{"Authorization":{"variable":"TOKEN"}}}),
        ] {
            assert!(serde_json::from_value::<ConfiguredServer>(value).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn non_unicode_references_fail_without_exposing_values() {
        use std::os::unix::ffi::OsStringExt;
        let raw = declaration(json!({"command":["server"],"env":{"TOKEN":{"env":"TOKEN"}}}));
        let error = raw
            .prepare(&Environment::from([(
                "TOKEN".into(),
                OsString::from_vec(vec![255]),
            )]))
            .unwrap_err();
        assert!(error.contains("not Unicode"));
    }

    #[cfg(unix)]
    #[test]
    fn executable_and_cwd_resolution_use_the_verified_root_and_frozen_daemon_path() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("bin")).unwrap();
        std::fs::create_dir(root.join("working")).unwrap();
        let executable = root.join("bin/server");
        std::fs::write(&executable, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let environment = Environment::from([("PATH".into(), "bin".into())]);
        let node = hive_core::identity::NodeId::new();
        for command in ["server", "./bin/server"] {
            let raw = declaration(
                json!({"command":[command,"literal"],"cwd":"working","env":{"PATH":"/not/the/lookup/path"}}),
            );
            let prepared = raw.prepare(&environment).unwrap();
            assert!(prepared.admission("files", node.clone(), None).is_err());
            let admission = prepared
                .admission("files", node.clone(), Some(&root))
                .unwrap();
            assert_eq!(
                Path::new(admission.launch.executable.as_str())
                    .canonicalize()
                    .unwrap(),
                executable
            );
            assert_eq!(
                Path::new(admission.launch.cwd.unwrap().as_str())
                    .canonicalize()
                    .unwrap(),
                root.join("working")
            );
            assert_eq!(
                admission.launch.env[OsStr::new("PATH")],
                "/not/the/lookup/path"
            );
            assert!(matches!(admission.sharing, Sharing::Private));
        }
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(lookup_executable(Path::new("server"), &environment, Some(&root)).is_err());
    }
}
