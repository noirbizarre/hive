//! Stable agent-facing contracts, versioned independently of ACP and Hive releases.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const CAPABILITIES: &str = "hive.workspace.capabilities";
pub const VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Tool {
    pub name: String,
    pub schema_version: u32,
    pub effect: Effect,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    ReadOnly,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Capabilities {
    pub schema_version: u32,
    pub owner: String,
    pub project: String,
    pub workspace: String,
    pub repository_backend: Option<String>,
    pub provisioner: String,
    pub tools: Vec<Tool>,
}

/// This catalog describes implemented handlers, not hypothetical backend functionality.
pub fn tools() -> Vec<Tool> {
    vec![Tool {
        name: CAPABILITIES.into(),
        schema_version: VERSION,
        effect: Effect::ReadOnly,
    }]
}

pub fn validate_arguments(arguments: Option<&Value>) -> Result<(), ToolError> {
    match arguments {
        None => Ok(()),
        Some(Value::Object(fields)) if fields.is_empty() => Ok(()),
        _ => Err(ToolError::Arguments),
    }
}

pub fn descriptor() -> Value {
    json!({
        "name": CAPABILITIES,
        "title": "Hive workspace capabilities",
        "description": "Discover callable Hive tools for this session's workspace and its pinned repository backend and provisioner.",
        "inputSchema": {"type":"object", "properties":{}, "additionalProperties":false},
        "outputSchema": {
            "type":"object",
            "required":["schemaVersion","owner","project","workspace","repositoryBackend","provisioner","tools"],
            "properties": {
                "schemaVersion":{"type":"integer","const":VERSION},
                "owner":{"type":"string"}, "project":{"type":"string"},
                "workspace":{"type":"string"}, "repositoryBackend":{"type":["string","null"]},
                "provisioner":{"type":"string"},
                "tools":{"type":"array","items":{
                    "type":"object", "required":["name","schemaVersion","effect"],
                    "properties":{"name":{"type":"string"},"schemaVersion":{"type":"integer"},
                        "effect":{"type":"string","enum":["read_only"]}}
                }}
            }
        },
        "annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false}
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ToolError {
    #[error("Workspace tool arguments must be an empty object")]
    Arguments,
    #[error("This Hive workspace tool is not supported")]
    Unsupported,
    #[error("The session's registered workspace scope is unavailable or stale")]
    Scope,
    #[error("The workspace belongs to another Hive node")]
    Owner,
    #[error("The workspace tool's session authority has ended")]
    Session,
    #[error("The workspace tool operation was cancelled")]
    Cancelled,
    #[error("Hive could not inspect the workspace resource store or backend")]
    Backend,
}

impl ToolError {
    pub fn code(self) -> &'static str {
        match self {
            Self::Arguments => "hive::workspace_tools::arguments",
            Self::Unsupported => "hive::workspace_tools::unsupported",
            Self::Scope => "hive::workspace_tools::scope",
            Self::Owner => "hive::workspace_tools::owner",
            Self::Session => "hive::workspace_tools::session",
            Self::Cancelled => "hive::workspace_tools::cancelled",
            Self::Backend => "hive::workspace_tools::backend",
        }
    }

    pub fn help(self) -> &'static str {
        match self {
            Self::Arguments => {
                "Omit arguments or supply {}; paths and resource selectors are not accepted"
            }
            Self::Unsupported => "Discover tools again and use an advertised Hive tool name",
            Self::Scope => {
                "Inspect the session's workspace registration and backend on its owning Hive node before retrying"
            }
            Self::Owner => "Reconnect to the Hive node that owns the session and workspace",
            Self::Session => {
                "Use the active session attachment; closed or replaced attachments cannot execute tools"
            }
            Self::Cancelled => {
                "Retry only from an active session if the read-only result is still needed"
            }
            Self::Backend => {
                "Inspect workspace diagnostics on the owning Hive node, repair the resource store or backend, then retry"
            }
        }
    }

    pub fn data(self) -> Value {
        json!({"code":self.code(),"message":self.to_string(),"help":self.help()})
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_cannot_select_another_workspace_or_path() {
        assert!(validate_arguments(None).is_ok());
        assert!(validate_arguments(Some(&json!({}))).is_ok());
        for invalid in [
            json!(null),
            json!([]),
            json!({"workspace":"other"}),
            json!({"path":"/tmp"}),
        ] {
            assert!(matches!(
                validate_arguments(Some(&invalid)),
                Err(ToolError::Arguments)
            ));
        }
    }

    #[test]
    fn discovery_only_advertises_the_implemented_versioned_read_only_handler() {
        let descriptor = descriptor();
        assert_eq!(descriptor["name"], tools()[0].name);
        assert_eq!(descriptor["inputSchema"]["additionalProperties"], false);
        assert_eq!(tools().len(), 1);
        let result = Capabilities {
            schema_version: VERSION,
            owner: "node".into(),
            project: "project".into(),
            workspace: "workspace".into(),
            repository_backend: None,
            provisioner: "external.directory".into(),
            tools: tools(),
        };
        let value = serde_json::to_value(&result).unwrap();
        for required in descriptor["outputSchema"]["required"].as_array().unwrap() {
            assert!(value.get(required.as_str().unwrap()).is_some());
        }
        assert_eq!(value["schemaVersion"], VERSION);
        assert_eq!(value["tools"][0]["effect"], "read_only");
        assert_eq!(
            serde_json::from_value::<Capabilities>(value).unwrap(),
            result
        );
    }
}
