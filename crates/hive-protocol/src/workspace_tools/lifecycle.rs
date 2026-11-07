//! Workspace selectors express intent; the owner-side service supplies all authority.

use super::*;
use hive_core::{
    identity::{OperationId, WorkspaceId},
    project::Revision,
    workspace::GitRef,
};

pub const NAMES: [&str; 6] = [
    "hive.workspace.current",
    "hive.workspace.info",
    "hive.workspace.list",
    "hive.workspace.create",
    "hive.workspace.remove",
    "hive.workspace.operation",
];

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Current,
    Info {
        workspace: Option<WorkspaceId>,
    },
    List {
        #[serde(default = "default_limit")]
        limit: usize,
        cursor: Option<WorkspaceId>,
    },
    Create {
        operation: OperationId,
        base_ref: Option<GitRef>,
        branch: Option<GitRef>,
        revision: Option<Revision>,
    },
    Remove {
        workspace: WorkspaceId,
        operation: OperationId,
    },
    Operation {
        operation: OperationId,
    },
}

fn default_limit() -> usize {
    20
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceInfo {
    pub schema_version: u32,
    pub owner: hive_core::identity::NodeId,
    pub project: hive_core::identity::ProjectId,
    pub workspace: WorkspaceId,
    pub state: hive_core::workspace::WorkspaceState,
    pub ownership: hive_core::workspace::Ownership,
    pub operation: Option<OperationId>,
    pub failure: Option<String>,
    pub retry: Option<hive_core::workspace::RetryDisposition>,
    pub repository_backend: Option<String>,
    pub provisioner: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceList {
    pub schema_version: u32,
    pub owner: hive_core::identity::NodeId,
    pub project: hive_core::identity::ProjectId,
    pub workspaces: Vec<WorkspaceInfo>,
    pub next_cursor: Option<WorkspaceId>,
}

impl Request {
    pub fn mutates(&self) -> bool {
        matches!(self, Self::Create { .. } | Self::Remove { .. })
    }
    pub fn operation(&self) -> Option<&OperationId> {
        match self {
            Self::Create { operation, .. }
            | Self::Remove { operation, .. }
            | Self::Operation { operation } => Some(operation),
            _ => None,
        }
    }
}

pub fn request(name: &str, arguments: Option<&Value>) -> Result<Request, ToolError> {
    if !NAMES.contains(&name) {
        return Err(ToolError::Unsupported);
    }
    let mut fields = match arguments {
        None => serde_json::Map::new(),
        Some(Value::Object(fields)) => fields.clone(),
        _ => return Err(ToolError::Arguments),
    };
    if fields.contains_key("action") || fields.values().any(Value::is_null) {
        return Err(ToolError::Arguments);
    }
    // Serde's internally tagged unit variants ignore extra keys even with deny_unknown_fields.
    if name == "hive.workspace.current" && !fields.is_empty() {
        return Err(ToolError::Arguments);
    }
    fields.insert(
        "action".into(),
        json!(name.strip_prefix("hive.workspace.").unwrap()),
    );
    let request: Request =
        serde_json::from_value(Value::Object(fields)).map_err(|_| ToolError::Arguments)?;
    match &request {
        Request::List { limit, .. } if !(1..=100).contains(limit) => {
            return Err(ToolError::Arguments);
        }
        Request::Create {
            base_ref,
            branch,
            revision,
            ..
        } if (revision.is_some() && (base_ref.is_some() || branch.is_some()))
            || base_ref.as_ref().is_some_and(|v| v.as_str().len() > 4096)
            || branch.as_ref().is_some_and(|v| v.as_str().len() > 4096)
            || revision.as_ref().is_some_and(|v| v.as_str().len() > 4096) =>
        {
            return Err(ToolError::Arguments);
        }
        _ => {}
    }
    Ok(request)
}

pub fn catalog(managed: bool) -> Vec<Tool> {
    NAMES
        .into_iter()
        .filter(|name| managed || *name != "hive.workspace.create")
        .map(|name| Tool {
            name: name.into(),
            schema_version: VERSION,
            effect: if matches!(name, "hive.workspace.create" | "hive.workspace.remove") {
                Effect::Mutation
            } else {
                Effect::ReadOnly
            },
        })
        .collect()
}

pub fn descriptor(name: &str) -> Value {
    let id = json!({"type":"string","pattern":"^workspace-[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$"});
    let operation = json!({"type":"string","pattern":"^operation-[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$"});
    let (properties, required) = match name {
        "hive.workspace.info" => (json!({"workspace":id}), vec![]),
        "hive.workspace.list" => (
            json!({"limit":{"type":"integer","minimum":1,"maximum":100,"default":20},"cursor":id}),
            vec![],
        ),
        "hive.workspace.create" => (
            json!({"operation":operation,"base_ref":{"type":"string","minLength":1,"maxLength":4096},"branch":{"type":"string","minLength":1,"maxLength":4096},"revision":{"type":"string","minLength":1,"maxLength":4096}}),
            vec!["operation"],
        ),
        "hive.workspace.remove" => (
            json!({"workspace":id,"operation":operation}),
            vec!["workspace", "operation"],
        ),
        "hive.workspace.operation" => (json!({"operation":operation}), vec!["operation"]),
        _ => (json!({}), vec![]),
    };
    let mutation = matches!(name, "hive.workspace.create" | "hive.workspace.remove");
    json!({"name":name,"description":match name {
        "hive.workspace.current" => "Inspect this session's attached workspace without probing the filesystem yourself.",
        "hive.workspace.info" => "Inspect the current workspace or an explicit sibling in the session's project on its owning node.",
        "hive.workspace.list" => "List a bounded page of authorized project workspaces; use nextCursor for the next page.",
        "hive.workspace.create" => "Create an unattached sibling using the current managed policy and pinned provisioner. Supply a fresh operation-<UUID> before dispatch. Git accepts base_ref/branch; JJ accepts revision. Retry only with the original ID and intent after inspecting operation status.",
        "hive.workspace.remove" => "Request preserve-only removal of an explicit workspace. Dirty, untracked, ignored, active, external and uncertain resources remain protected. Supply an operation-<UUID>; inspect operation status before retrying.",
        _ => "Inspect a committed operation snapshot without replaying effects. Lifecycle state is not proof a worker is alive; missing is not permission to change the operation ID."
    },"inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":false},
    "outputSchema":output_schema(name),
    "annotations":{"readOnlyHint":!mutation,"destructiveHint":name == "hive.workspace.remove","idempotentHint":true,"openWorldHint":false}})
}

fn output_schema(name: &str) -> Value {
    let workspace = json!({"type":"object","required":["schemaVersion","owner","project","workspace","state","ownership","operation","failure","retry","repositoryBackend","provisioner"],
        "properties":{"schemaVersion":{"type":"integer","const":VERSION},"owner":{"type":"string"},"project":{"type":"string"},"workspace":{"type":"string"},
            "state":{"type":"string"},"ownership":{"type":"string","enum":["hive_managed","external"]},
            "operation":{"type":["string","null"]},"failure":{"type":["string","null"]},"retry":{"type":["string","null"]},
            "repositoryBackend":{"type":["string","null"]},"provisioner":{"type":["string","null"]}}});
    if name == "hive.workspace.list" {
        return json!({"type":"object","required":["schemaVersion","owner","project","workspaces","nextCursor"],"properties":{
            "schemaVersion":{"type":"integer","const":VERSION},"owner":{"type":"string"},"project":{"type":"string"},
            "workspaces":{"type":"array","maxItems":100,"items":workspace},"nextCursor":{"type":["string","null"]}}});
    }
    let mut schema = workspace;
    if matches!(name, "hive.workspace.current" | "hive.workspace.info") {
        schema["properties"]["inspection"] = json!({"type":"object","required":["presence"],"properties":{
            "presence":{"type":"string","enum":["absent","present","partial","identity_conflict"]},
            "safety":{"type":"object","required":["ownership_verified","cleanliness","usage"],"properties":{
                "ownership_verified":{"type":"boolean"},"cleanliness":{"type":"string","enum":["clean","dirty","unknown"]},"usage":{"type":"string","enum":["idle","active","unknown"]}}}}});
        schema["required"]
            .as_array_mut()
            .unwrap()
            .push(json!("inspection"));
    }
    if name == "hive.workspace.operation" {
        schema["properties"]["kind"] =
            json!({"type":["string","null"],"enum":["create","cleanup",null]});
        schema["properties"]["effectsStarted"] = json!({"type":"boolean"});
        schema["properties"]["persistedSnapshot"] = json!({"type":"boolean","const":true});
        schema["required"].as_array_mut().unwrap().extend([
            json!("kind"),
            json!("effectsStarted"),
            json!("persistedSnapshot"),
        ]);
    }
    schema
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_arguments_accept_only_typed_selectors_and_bounded_revision_intent() {
        let operation = OperationId::new();
        assert!(request("hive.workspace.current", None).is_ok());
        assert!(request("hive.workspace.info", Some(&json!({}))).is_ok());
        assert!(
            request(
                "hive.workspace.create",
                Some(&json!({"operation":operation}))
            )
            .unwrap()
            .mutates()
        );
        for arguments in [
            json!(null),
            json!({"workspace":"other"}),
            json!({"path":"/tmp"}),
            json!({"action":"current"}),
        ] {
            assert_eq!(
                request("hive.workspace.current", Some(&arguments)).unwrap_err(),
                ToolError::Arguments
            );
        }
        for arguments in [
            json!({"operation":operation,"discard":true}),
            json!({"operation":operation,"backend":"git.worktree"}),
            json!({"operation":operation,"revision":"trunk()","branch":"task"}),
            json!({"operation":operation,"base_ref":null}),
            json!({"operation":operation,"base_ref":"-option"}),
            json!({"operation":operation,"revision":"x".repeat(4097)}),
        ] {
            assert_eq!(
                request("hive.workspace.create", Some(&arguments)).unwrap_err(),
                ToolError::Arguments
            );
        }
        for limit in [0, 101] {
            assert!(request("hive.workspace.list", Some(&json!({"limit":limit}))).is_err());
        }
        assert!(
            request(
                "hive.workspace.remove",
                Some(&json!({"operation":operation}))
            )
            .is_err()
        );
    }

    #[test]
    fn lifecycle_discovery_distinguishes_managed_creation_and_preserve_only_mutations() {
        assert_eq!(catalog(false).len(), 5);
        assert_eq!(catalog(true).len(), 6);
        for tool in catalog(true) {
            let schema = descriptor(&tool.name);
            assert_eq!(schema["inputSchema"]["additionalProperties"], false);
            assert_eq!(
                schema["annotations"]["readOnlyHint"],
                tool.effect == Effect::ReadOnly
            );
            assert!(schema["inputSchema"]["properties"].get("discard").is_none());
            assert!(schema["inputSchema"]["properties"].get("path").is_none());
        }
    }
}
