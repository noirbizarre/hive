//! Managed MCP contracts, configured routing and the shared stdio runtime.
//!
//! Identity validation is pure; scoped Broker admission observes registered local
//! resources while trusted callers supply effective environment and security evidence.

use std::{collections::BTreeMap, ffi::OsString, fmt};

use axum::http::{HeaderName, HeaderValue};
use hive_core::{
    identity::{NodeId, ProjectId, WorkspaceId},
    workspace::NodePath,
};
use miette::Diagnostic;
use serde::{Deserialize, Serialize};

pub(crate) mod configuration;
mod namespace;
mod pagination;
mod process;
mod resolution;
pub(crate) mod routing;
pub use resolution::{AdmissionError, Sharing, StdioAdmission, StdioLaunch};
pub mod runtime;
mod sessions;
pub(crate) use sessions::PendingTarget;
pub use sessions::SessionMcpLease;
pub(crate) use sessions::Sessions;
pub(crate) use sessions::StartupOwnership;
pub(crate) use sessions::settled;

#[cfg(test)]
mod tests;

/// Scope limits eligibility; it never overrides configuration or credential isolation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Machine,
    Project,
    Workspace,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ownership {
    Forwarded,
    Managed,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Lifecycle {
    Delegated,
    /// Zero means stop after the last lease and operation; active users prevent cleanup.
    OnDemand {
        idle_timeout_seconds: u64,
    },
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Transport {
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
    },
    Http {
        url: String,
        #[serde(default)]
        headers: BTreeMap<String, String>,
    },
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DefinitionRecord {
    transport: Transport,
    scope: Scope,
    ownership: Ownership,
    lifecycle: Lifecycle,
}

/// A checked standalone schema. Deserializing this does not start or register a server.
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "DefinitionRecord", into = "DefinitionRecord")]
pub struct ServerDefinition(DefinitionRecord);

/// Controlled diagnostics never include commands, URLs, headers or credential values.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error, Diagnostic)]
pub enum McpError {
    #[error(
        "invalid MCP process configuration; use a nonblank command, valid environment names and no NUL bytes"
    )]
    #[diagnostic(code(hive::mcp::process))]
    Process,
    #[error(
        "invalid MCP HTTP configuration; use an HTTP(S) URL with a host, no userinfo or fragment, and valid unambiguous headers"
    )]
    #[diagnostic(code(hive::mcp::http))]
    Http,
    #[error(
        "managed MCP HTTP is not supported; forward an existing HTTP endpoint or use managed stdio"
    )]
    #[diagnostic(code(hive::mcp::managed_http))]
    ManagedHttp,
    #[error(
        "incompatible MCP lifecycle; use delegated for forwarded servers or on_demand for managed stdio"
    )]
    #[diagnostic(code(hive::mcp::lifecycle))]
    Lifecycle,
    #[error(
        "missing or inconsistent MCP scope context; supply the registered project/workspace on the execution node"
    )]
    #[diagnostic(code(hive::mcp::scope))]
    Scope,
    #[error(
        "inconsistent MCP resolved configuration; supply matching transport, node-local paths and all explicit overrides"
    )]
    #[diagnostic(code(hive::mcp::resolution))]
    Resolution,
    #[error(
        "incomplete MCP security context; supply a principal, partition, policy generation and trusted credential generations, or use a private identity"
    )]
    #[diagnostic(code(hive::mcp::security))]
    Security,
}

fn nonblank(value: &str) -> bool {
    !value.trim().is_empty() && !value.contains('\0')
}

fn valid_env_name(value: &str) -> bool {
    !value.is_empty() && !value.contains(['=', '\0'])
}

fn validate_http(url: &str, headers: &BTreeMap<String, String>) -> Result<(), McpError> {
    let parsed = reqwest::Url::parse(url).map_err(|_| McpError::Http)?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.fragment().is_some()
    {
        return Err(McpError::Http);
    }
    let mut names = std::collections::BTreeSet::new();
    for (name, value) in headers {
        let name = HeaderName::try_from(name.as_str()).map_err(|_| McpError::Http)?;
        // Differently cased duplicates otherwise have ambiguous effective semantics.
        if !names.insert(name.as_str().to_owned()) || HeaderValue::try_from(value.as_str()).is_err()
        {
            return Err(McpError::Http);
        }
    }
    Ok(())
}

impl ServerDefinition {
    pub fn new(
        transport: Transport,
        scope: Scope,
        ownership: Ownership,
        lifecycle: Lifecycle,
    ) -> Result<Self, McpError> {
        Self::try_from(DefinitionRecord {
            transport,
            scope,
            ownership,
            lifecycle,
        })
    }

    pub fn transport(&self) -> &Transport {
        &self.0.transport
    }
    pub fn scope(&self) -> Scope {
        self.0.scope
    }
    pub fn ownership(&self) -> Ownership {
        self.0.ownership
    }
    pub fn lifecycle(&self) -> &Lifecycle {
        &self.0.lifecycle
    }
}

impl TryFrom<DefinitionRecord> for ServerDefinition {
    type Error = McpError;

    fn try_from(record: DefinitionRecord) -> Result<Self, Self::Error> {
        match &record.transport {
            Transport::Stdio { command, args, env } => {
                if !nonblank(command)
                    || args.iter().any(|arg| arg.contains('\0'))
                    || env
                        .iter()
                        .any(|(key, value)| !valid_env_name(key) || value.contains('\0'))
                {
                    return Err(McpError::Process);
                }
            }
            Transport::Http { url, headers } => {
                validate_http(url, headers)?;
                if record.ownership == Ownership::Managed {
                    return Err(McpError::ManagedHttp);
                }
            }
        }
        if !matches!(
            (&record.ownership, &record.lifecycle),
            (Ownership::Forwarded, Lifecycle::Delegated)
                | (Ownership::Managed, Lifecycle::OnDemand { .. })
        ) {
            return Err(McpError::Lifecycle);
        }
        Ok(Self(record))
    }
}

impl From<ServerDefinition> for DefinitionRecord {
    fn from(value: ServerDefinition) -> Self {
        value.0
    }
}

/// Resource ownership must be verified by the resolver, not inferred from cwd.
#[derive(Clone, Debug)]
pub struct ScopeContext {
    pub node: NodeId,
    pub project: Option<ProjectId>,
    pub workspace: Option<WorkspaceId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ScopeIdentity {
    Machine(NodeId),
    Project(NodeId, ProjectId),
    Workspace(NodeId, ProjectId, WorkspaceId),
}

/// Full effective inputs, including inherited environment, not just declared overrides.
#[derive(Clone, Eq, PartialEq)]
pub enum ResolvedTransport {
    Stdio {
        executable: NodePath,
        args: Vec<String>,
        env: BTreeMap<OsString, OsString>,
        cwd: NodePath,
    },
    /// Forwarding equality does not authorize pooling a remote MCP session.
    Http {
        url: String,
        headers: BTreeMap<String, String>,
        /// Effective proxy/TLS/client-certificate configuration or trusted generations.
        network_context: BTreeMap<String, String>,
    },
}

/// Include everything sent during initialization; unknown options remain significant.
#[derive(Clone, Eq, PartialEq)]
pub struct Initialization {
    pub protocol_version: String,
    pub client_info: serde_json::Value,
    pub capabilities: serde_json::Value,
    pub options: serde_json::Value,
    pub roots: Vec<NodePath>,
}

#[derive(Clone, Eq, PartialEq)]
pub enum Credentials {
    /// The caller certifies all material is in effective inputs and all external
    /// sources have trusted generations that change on credential rotation.
    Complete {
        external_generations: BTreeMap<String, String>,
    },
    /// Unknown external state cannot authorize shared reuse.
    Unknown,
}

#[derive(Clone, Eq, PartialEq)]
pub struct SecurityContext {
    pub principal: String,
    pub partition: String,
    pub policy_generation: String,
    pub credentials: Credentials,
}

/// Private fields prevent callers from bypassing validation or exposing secrets.
/// Structural equality avoids publishing an offline guessing oracle for credentials.
#[derive(Clone, Eq, PartialEq)]
pub struct InstanceIdentity {
    definition_id: String,
    definition: ServerDefinition,
    pub(crate) scope: ScopeIdentity,
    pub(crate) transport: ResolvedTransport,
    pub(crate) initialization: Initialization,
    security: SecurityContext,
    private_nonce: Option<uuid::Uuid>,
}

impl InstanceIdentity {
    /// Build a reusable key only from a complete, trusted resolution snapshot.
    pub fn shared(
        definition_id: String,
        definition: ServerDefinition,
        context: ScopeContext,
        transport: ResolvedTransport,
        initialization: Initialization,
        security: SecurityContext,
    ) -> Result<Self, McpError> {
        Self::build(
            definition_id,
            definition,
            context,
            transport,
            initialization,
            security,
            false,
        )
    }

    /// A fresh nonce prevents independently admitted instances with unknown
    /// credential state from sharing, even if their visible configuration matches.
    pub fn private(
        definition_id: String,
        definition: ServerDefinition,
        context: ScopeContext,
        transport: ResolvedTransport,
        initialization: Initialization,
        security: SecurityContext,
    ) -> Result<Self, McpError> {
        Self::build(
            definition_id,
            definition,
            context,
            transport,
            initialization,
            security,
            true,
        )
    }

    fn build(
        definition_id: String,
        definition: ServerDefinition,
        context: ScopeContext,
        transport: ResolvedTransport,
        initialization: Initialization,
        security: SecurityContext,
        private: bool,
    ) -> Result<Self, McpError> {
        if !nonblank(&security.principal)
            || !nonblank(&security.partition)
            || !nonblank(&security.policy_generation)
        {
            return Err(McpError::Security);
        }
        match &security.credentials {
            Credentials::Unknown if !private => return Err(McpError::Security),
            Credentials::Complete {
                external_generations,
            } if external_generations
                .iter()
                .any(|(source, generation)| !nonblank(source) || !nonblank(generation)) =>
            {
                return Err(McpError::Security);
            }
            _ => {}
        }
        let scope = match definition.scope() {
            Scope::Machine => ScopeIdentity::Machine(context.node.clone()),
            Scope::Project => ScopeIdentity::Project(
                context.node.clone(),
                context.project.ok_or(McpError::Scope)?,
            ),
            Scope::Workspace => ScopeIdentity::Workspace(
                context.node.clone(),
                context.project.ok_or(McpError::Scope)?,
                context.workspace.ok_or(McpError::Scope)?,
            ),
        };
        if !nonblank(&definition_id)
            || !nonblank(&initialization.protocol_version)
            || initialization
                .roots
                .iter()
                .any(|root| root.node() != &context.node)
        {
            return Err(McpError::Resolution);
        }
        match (definition.transport(), &transport) {
            (
                Transport::Stdio { args, env, .. },
                ResolvedTransport::Stdio {
                    executable,
                    args: effective_args,
                    env: effective_env,
                    cwd,
                },
            ) => {
                if executable.node() != &context.node
                    || cwd.node() != &context.node
                    || args != effective_args
                    || effective_env.iter().any(|(key, value)| {
                        key.is_empty()
                            || key.as_encoded_bytes().contains(&b'=')
                            || key.as_encoded_bytes().contains(&0)
                            || value.as_encoded_bytes().contains(&0)
                    })
                    || env.iter().any(|(key, value)| {
                        effective_env.get(&OsString::from(key)) != Some(&OsString::from(value))
                    })
                {
                    return Err(McpError::Resolution);
                }
            }
            (
                Transport::Http { url, headers },
                ResolvedTransport::Http {
                    url: effective_url,
                    headers: effective_headers,
                    ..
                },
            ) => {
                validate_http(effective_url, effective_headers)?;
                if url != effective_url
                    || headers.iter().any(|(key, value)| {
                        // HTTP resolvers may normalize names; casing cannot invalidate
                        // an explicit override whose effective value is unchanged.
                        !effective_headers
                            .iter()
                            .any(|(effective_key, effective_value)| {
                                key.eq_ignore_ascii_case(effective_key) && value == effective_value
                            })
                    })
                {
                    return Err(McpError::Resolution);
                }
            }
            _ => return Err(McpError::Resolution),
        }
        Ok(Self {
            definition_id,
            definition,
            scope,
            transport,
            initialization,
            security,
            private_nonce: private.then(uuid::Uuid::new_v4),
        })
    }
}

// Redact entire aggregates: URLs, initialization options and even env names can
// contain credentials. Adding a field must not silently expand diagnostic output.
macro_rules! redacted_debug {
    ($($ty:ty),+ $(,)?) => { $(impl fmt::Debug for $ty {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(concat!(stringify!($ty), "([redacted])"))
        }
    })+ };
}
redacted_debug!(
    Transport,
    DefinitionRecord,
    ServerDefinition,
    ResolvedTransport,
    Initialization,
    Credentials,
    SecurityContext,
    InstanceIdentity
);
