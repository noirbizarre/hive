//! Stable agent-facing contracts, versioned independently of ACP and Hive releases.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const CAPABILITIES: &str = "hive.workspace.capabilities";
pub const VERSION: u32 = 1;
pub const VCS_TOOLS: [&str; 4] = [
    "hive.vcs.status",
    "hive.vcs.diff",
    "hive.vcs.commit",
    "hive.vcs.log",
];
pub use hive_core::workspace::backend::vcs;

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
    Mutation,
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

pub fn catalog(supports_vcs: bool) -> Vec<Tool> {
    let mut catalog = tools();
    if supports_vcs {
        catalog.extend(VCS_TOOLS.into_iter().map(|name| Tool {
            name: name.into(),
            schema_version: VERSION,
            effect: if name == "hive.vcs.commit" {
                Effect::Mutation
            } else {
                Effect::ReadOnly
            },
        }));
    }
    catalog
}

pub fn request(name: &str, arguments: Option<&Value>) -> Result<vcs::Request, ToolError> {
    let operation = name
        .strip_prefix("hive.vcs.")
        .filter(|_| VCS_TOOLS.contains(&name))
        .ok_or(ToolError::Unsupported)?;
    let mut fields = match arguments {
        None => serde_json::Map::new(),
        Some(Value::Object(fields)) => fields.clone(),
        _ => return Err(ToolError::Arguments),
    };
    // The operation comes from the literal tool name, never an argument override.
    if fields.contains_key("operation") {
        return Err(ToolError::Arguments);
    }
    fields.insert("operation".into(), json!(operation));
    let request: vcs::Request =
        serde_json::from_value(Value::Object(fields)).map_err(|_| ToolError::Arguments)?;
    request.validate().map_err(|_| ToolError::Arguments)?;
    Ok(request)
}

pub fn tool_descriptor(name: &str) -> Value {
    if name == CAPABILITIES {
        return descriptor();
    }
    let mut properties = json!({});
    let mut required = vec![];
    match name {
        "hive.vcs.diff" => {
            properties = json!({"view":{"type":"string","enum":["staged","unstaged","working_copy"],"default":"working_copy"},"max_bytes":{"type":"integer","minimum":1,"maximum":262144,"default":65536}})
        }
        "hive.vcs.log" => {
            properties = json!({"limit":{"type":"integer","minimum":1,"maximum":100,"default":20}})
        }
        "hive.vcs.commit" => {
            required = vec!["message", "inclusion"];
            properties = json!({"message":{"type":"string","minLength":1,"maxLength":16384},"inclusion":{"type":"string","enum":["staged_only","tracked_working_copy"]},"untracked":{"type":"array","items":{"type":"string"},"maxItems":1000},"ignored":{"type":"array","items":{"type":"string"},"maxItems":1000},"expected_state":{"type":"string"}});
        }
        _ => {}
    }
    let mutation = name == "hive.vcs.commit";
    json!({"name":name,"description":match name {
        "hive.vcs.status" => "Inspect tracked, untracked and ignored workspace state without snapshotting files.",
        "hive.vcs.diff" => "Inspect bounded tracked diffs. JJ returns separate recorded-change and physical-edit layers; staged view is Git-only.",
        "hive.vcs.commit" => "Record changes with explicit inclusion. Git supports staged_only and tracked_working_copy; JJ supports tracked_working_copy. New files require literal untracked/ignored lists. Inspect status/log before retrying interrupted mutations.",
        _ => "Read bounded recent native change history, newest first, including the recorded JJ working-copy change."
    },"inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":false},
    "outputSchema":vcs_output_schema(),"annotations":{"readOnlyHint":!mutation,"destructiveHint":mutation,"idempotentHint":!mutation,"openWorldHint":false}})
}

fn vcs_output_schema() -> Value {
    json!({
        "type":"object",
        "required":["schemaVersion","operation","owner","project","workspace","repository","repositoryBackend","outcome","state","recorded","diff","history"],
        "properties":{
            "schemaVersion":{"type":"integer","const":VERSION},
            "operation":{"type":"string","enum":["status","diff","commit","log"]},
            "owner":{"type":"string"},"project":{"type":"string"},"workspace":{"type":"string"},"repository":{"type":"string"},"repositoryBackend":{"type":"string"},
            "outcome":{"type":"string","enum":["complete","recorded","recorded_concurrent","recorded_state_unavailable"]},
            "state":{"$ref":"#/$defs/state"},
            "recorded":{"anyOf":[{"$ref":"#/$defs/change"},{"type":"null"}]},
            "diff":{"type":"array","items":{"$ref":"#/$defs/section"}},
            "history":{"type":"array","maxItems":100,"items":{"$ref":"#/$defs/change"}}
        },
        "$defs":{
            "identity":{"type":"object","required":["commitId","changeId","operationId"],"properties":{
                "commitId":{"type":"string"},"changeId":{"type":["string","null"]},"operationId":{"type":["string","null"]}
            }},
            "change":{"type":"object","required":["identity","message","messageTruncated"],"properties":{
                "identity":{"$ref":"#/$defs/identity"},"message":{"type":"string"},"messageTruncated":{"type":"boolean"}
            }},
            "state":{"type":"object","required":["identity","branch","stateToken","dirty","recordedChange","conflicts","files","complete"],"properties":{
                "identity":{"$ref":"#/$defs/identity"},"branch":{"type":["string","null"]},"stateToken":{"type":"string"},"dirty":{"type":"boolean"},"recordedChange":{"type":"boolean"},"conflicts":{"type":"boolean"},"complete":{"type":"boolean"},
                "files":{"type":"array","maxItems":1000,"items":{"$ref":"#/$defs/file"}}
            }},
            "file":{"type":"object","required":["path","originalPath","index","workingCopy","untracked","ignored","conflict"],"properties":{
                "path":{"type":"string"},"originalPath":{"type":["string","null"]},"index":{"type":["string","null"]},"workingCopy":{"type":["string","null"]},"untracked":{"type":"boolean"},"ignored":{"type":"boolean"},"conflict":{"type":"boolean"}
            }},
            "section":{"type":"object","required":["layer","text","truncated"],"properties":{
                "layer":{"type":"string"},"text":{"type":"string"},"truncated":{"type":"boolean"}
            }}
        }
    })
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
                        "effect":{"type":"string","enum":["read_only","mutation"]}}
                }}
            }
        },
        "annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false}
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ToolError {
    #[error("Workspace tool arguments do not match its advertised schema")]
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
    #[error(transparent)]
    Vcs(#[from] vcs::Error),
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
            Self::Vcs(error) => match error {
                vcs::Error::Unsupported => "hive::vcs::unsupported",
                vcs::Error::Arguments => "hive::vcs::arguments",
                vcs::Error::Empty => "hive::vcs::empty",
                vcs::Error::Conflict => "hive::vcs::conflict",
                vcs::Error::Concurrent => "hive::vcs::concurrent",
                vcs::Error::Backend => "hive::vcs::backend",
                vcs::Error::Uncertain => "hive::vcs::uncertain",
                vcs::Error::Cancelled => "hive::vcs::cancelled",
            },
        }
    }

    pub fn help(self) -> &'static str {
        match self {
            Self::Arguments => {
                "Use the tool's advertised input schema; resource selectors are not accepted"
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
            Self::Vcs(vcs::Error::Uncertain) => {
                "Inspect status and native history on the owning node before retrying; staging or snapshotting may already have occurred"
            }
            Self::Vcs(vcs::Error::Concurrent) => {
                "Inspect status again and use its current state token; external writers are not locked out"
            }
            Self::Vcs(vcs::Error::Empty) => "Select existing changes explicitly before recording",
            Self::Vcs(vcs::Error::Conflict) => {
                "Resolve conflicts and complete any in-progress SCM workflow before retrying"
            }
            Self::Vcs(vcs::Error::Unsupported) => {
                "Use a supported backend inclusion mode or diff view; inspect backend diagnostics for unsupported file types"
            }
            Self::Vcs(vcs::Error::Arguments) => {
                "Check the input schema and supply literal workspace-relative file paths"
            }
            Self::Vcs(vcs::Error::Backend) => {
                "Inspect native SCM configuration and diagnostics on the owning node"
            }
            Self::Vcs(vcs::Error::Cancelled) => {
                "Use an active session attachment; this attempt dispatched no mutation"
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
    fn commits_require_explicit_inclusion_and_cannot_override_session_scope() {
        for arguments in [
            json!({"message":"work"}),
            json!({"message":"work","inclusion":"tracked_working_copy","workspace":"other"}),
            json!({"operation":"status"}),
            json!({"message":"work","inclusion":"staged_only","untracked":["new"]}),
            json!({"message":" ","inclusion":"staged_only"}),
        ] {
            assert_eq!(
                request("hive.vcs.commit", Some(&arguments)).unwrap_err(),
                ToolError::Arguments
            );
        }
        assert!(
            request(
                "hive.vcs.commit",
                Some(&json!({"message":"work","inclusion":"staged_only"}))
            )
            .unwrap()
            .mutates()
        );
        assert!(request("hive.vcs.status", None).is_ok());
        assert!(request("hive.vcs.log", Some(&json!({"limit":101}))).is_err());
        assert!(request("hive.vcs.diff", Some(&json!({"max_bytes":262145}))).is_err());
    }

    #[test]
    fn vcs_discovery_labels_only_commit_as_a_mutation_and_external_directories_have_no_vcs_tools() {
        assert_eq!(catalog(false), tools());
        let catalog = catalog(true);
        assert_eq!(catalog.len(), 5);
        for tool in catalog {
            let descriptor = tool_descriptor(&tool.name);
            assert_eq!(descriptor["name"], tool.name);
            assert_eq!(
                descriptor["annotations"]["readOnlyHint"],
                tool.effect == Effect::ReadOnly
            );
            assert_eq!(descriptor["inputSchema"]["additionalProperties"], false);
        }
    }

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
