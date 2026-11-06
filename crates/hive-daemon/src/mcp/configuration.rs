//! Operator configuration supplies trust; downstream metadata never authorizes sharing.

use std::{collections::BTreeMap, path::PathBuf};

use serde::Deserialize;

use super::*;

#[derive(Clone, Deserialize)]
#[serde(tag = "transport", rename_all = "snake_case", deny_unknown_fields)]
pub enum ConfiguredServer {
    Stdio {
        command: PathBuf,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
        scope: Scope,
        cwd: Option<PathBuf>,
        #[serde(default = "idle_timeout")]
        idle_timeout_seconds: u64,
        sharing: Option<SharedPolicy>,
    },
    Http {
        url: String,
        #[serde(default)]
        headers: BTreeMap<String, String>,
    },
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

fn idle_timeout() -> u64 {
    60
}

impl fmt::Debug for ConfiguredServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ConfiguredServer([redacted])")
    }
}

impl ConfiguredServer {
    pub fn managed(&self) -> bool {
        matches!(self, Self::Stdio { .. })
    }

    pub fn definition(&self) -> Result<ServerDefinition, McpError> {
        match self {
            Self::Stdio {
                command,
                args,
                env,
                scope,
                idle_timeout_seconds,
                ..
            } => ServerDefinition::new(
                Transport::Stdio {
                    command: command.to_str().ok_or(McpError::Process)?.into(),
                    args: args.clone(),
                    env: env.clone(),
                },
                *scope,
                Ownership::Managed,
                Lifecycle::OnDemand {
                    idle_timeout_seconds: *idle_timeout_seconds,
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

    pub fn validate(&self) -> Result<(), &'static str> {
        self.definition().map_err(
            |_| "invalid MCP transport; check command, arguments, environment or HTTP endpoint",
        )?;
        if let Self::Stdio {
            command,
            scope,
            cwd,
            sharing,
            ..
        } = self
        {
            if !command.is_absolute()
                || cwd.as_ref().is_some_and(|p| !p.is_absolute())
                || (*scope != Scope::Workspace && cwd.is_none())
            {
                return Err(
                    "managed MCP requires an absolute command and an absolute cwd for machine/project scope",
                );
            }
            if let Some(SharedPolicy::Shared {
                principal,
                partition,
                policy_generation,
                external_generations,
            }) = sharing
                && (!nonblank(principal)
                    || !nonblank(partition)
                    || !nonblank(policy_generation)
                    || external_generations
                        .iter()
                        .any(|(k, v)| !nonblank(k) || !nonblank(v)))
            {
                return Err(
                    "shared MCP requires nonblank principal, partition, policy and credential generations",
                );
            }
        }
        Ok(())
    }

    pub fn admission(
        &self,
        id: &str,
        node: hive_core::identity::NodeId,
    ) -> Result<StdioAdmission, AdmissionError> {
        let Self::Stdio {
            command,
            env,
            cwd,
            sharing,
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
        let path = |p: &PathBuf| {
            hive_core::workspace::NodePath::new(
                node.clone(),
                syntax,
                p.to_str().ok_or(AdmissionError::Path)?.into(),
            )
            .map_err(|_| AdmissionError::Path)
        };
        let (security, sharing) = match sharing {
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
        let mut effective: BTreeMap<_, _> = std::env::vars_os().collect();
        effective.extend(env.iter().map(|(k, v)| (k.into(), v.into())));
        Ok(StdioAdmission {
            definition_id: id.into(),
            definition: self.definition()?,
            launch: StdioLaunch {
                executable: path(command)?,
                cwd: cwd.as_ref().map(path).transpose()?,
                env: effective,
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
