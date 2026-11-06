//! Negotiated Hive discovery and selection; execution remains ordinary ACP.

use agent_client_protocol::{
    Agent, Error, JsonRpcMessage, JsonRpcRequest, UntypedMessage, V2ConnectionTo,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{AgentVersion, v2};

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RemoteError {
    #[error("Hive extension parameters are malformed or the inventory cursor has expired")]
    InvalidParams,
    #[error("The requested Hive feature was not negotiated or is unsupported")]
    Unsupported,
    #[error("The requested agent or configured default is unavailable")]
    UnknownAgent,
    #[error("The operation is not authorized by the owner")]
    Forbidden,
    #[error("The owner rejected the requested workspace")]
    InvalidWorkspace,
    #[error("The project definition conflicts with the execution node")]
    ProjectConflict,
    #[error("The execution node could not provision the workspace")]
    ProvisioningFailed,
    #[error("The execution node's workspace provisioning outcome is uncertain")]
    ProvisioningUncertain,
    #[error("The agent failed before session creation")]
    LaunchFailed,
    #[error("The agent startup outcome is uncertain")]
    LaunchUncertain,
}

impl RemoteError {
    pub fn code(self) -> &'static str {
        match self {
            Self::InvalidParams => "hive::remote::invalid_params",
            Self::Unsupported => "hive::remote::unsupported",
            Self::UnknownAgent => "hive::remote::unknown_agent",
            Self::Forbidden => "hive::remote::forbidden",
            Self::InvalidWorkspace => "hive::remote::invalid_workspace",
            Self::ProjectConflict => "hive::remote::project_conflict",
            Self::ProvisioningFailed => "hive::remote::provisioning_failed",
            Self::ProvisioningUncertain => "hive::remote::provisioning_uncertain",
            Self::LaunchFailed => "hive::remote::launch_failed",
            Self::LaunchUncertain => "hive::remote::launch_uncertain",
        }
    }

    pub fn help(self) -> &'static str {
        match self {
            Self::InvalidParams => {
                "Correct the Hive parameters, or restart discovery without a cursor."
            }
            Self::Unsupported => {
                "Inspect initialization capabilities and request supported features; do not drop a requested selector."
            }
            Self::UnknownAgent => {
                "Refresh the owner inventory and select an available agent explicitly."
            }
            Self::Forbidden => "Request access from the owner administrator.",
            Self::ProjectConflict => {
                "Select another project ID or explicitly update the definition on the execution node."
            }
            Self::ProvisioningFailed => {
                "Check the execution node's project, workspace policy, Git credentials and backend configuration; inspect its workspace operations before retrying."
            }
            Self::ProvisioningUncertain => {
                "Inspect and reconcile workspace operations on the execution node before an explicit retry; do not automatically provision another workspace."
            }
            Self::InvalidWorkspace => {
                "Select an owner-registered or transferred project with a compatible owner policy, or an existing absolute directory on the owner."
            }
            Self::LaunchFailed => {
                "Check the owner agent configuration and sanitized diagnostics before retrying."
            }
            Self::LaunchUncertain => {
                "Inspect the owner session catalog before an explicit retry; do not automatically create another session."
            }
        }
    }

    pub fn wire(self) -> Error {
        let numeric = match self {
            Self::InvalidParams | Self::UnknownAgent | Self::InvalidWorkspace => -32602,
            _ => -32060,
        };
        Error::new(numeric, self.to_string())
            .data(json!({"code": self.code(), "help": self.help()}))
    }

    /// Remote messages/data are untrusted, even when their diagnostic code is recognized.
    pub fn sanitized(error: Error, fallback: Self) -> Error {
        let code = error
            .data
            .as_ref()
            .and_then(|data| data.get("code"))
            .and_then(Value::as_str);
        [
            Self::InvalidParams,
            Self::Unsupported,
            Self::UnknownAgent,
            Self::Forbidden,
            Self::InvalidWorkspace,
            Self::ProjectConflict,
            Self::ProvisioningFailed,
            Self::ProvisioningUncertain,
            Self::LaunchFailed,
            Self::LaunchUncertain,
        ]
        .into_iter()
        .find(|kind| Some(kind.code()) == code)
        .unwrap_or(fallback)
        .wire()
    }
}

/// Inspect the raw container before the SDK's permissive metadata conversion can erase it.
pub fn hive_metadata(container: &Value) -> Result<Option<&Value>, Error> {
    match container.get("_meta") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(meta)) => Ok(meta.get("hive").filter(|value| !value.is_null())),
        _ => Err(RemoteError::InvalidParams.wire()),
    }
}

pub fn offered_versions(params: &Value) -> Result<Option<Vec<u64>>, Error> {
    let Some(hive) = hive_metadata(&params["capabilities"])? else {
        return Ok(None);
    };
    let object = hive
        .as_object()
        .ok_or_else(|| RemoteError::InvalidParams.wire())?;
    let versions = object
        .get("versions")
        .and_then(Value::as_array)
        .filter(|versions| !versions.is_empty())
        .ok_or_else(|| RemoteError::InvalidParams.wire())?;
    let mut result = Vec::new();
    for value in versions {
        let version = value
            .as_u64()
            .filter(|version| *version > 0)
            .ok_or_else(|| RemoteError::InvalidParams.wire())?;
        if result.contains(&version) {
            return Err(RemoteError::InvalidParams.wire());
        }
        result.push(version);
    }
    Ok(Some(result))
}

pub fn advertisement(node: &str) -> Value {
    json!({"version": 1, "node": node, "features": {"agents": {}, "selection": {}, "control": {}, "workspaces": {}}})
}

/// Retain raw values and redact Debug: malformed selectors must not become a default launch.
macro_rules! raw_request {
    ($name:ident, $method:literal, $response:ty) => {
        #[derive(Clone)]
        pub struct $name(pub Value);
        impl std::fmt::Debug for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(concat!(stringify!($name), " { .. }"))
            }
        }
        impl JsonRpcMessage for $name {
            fn matches_method(method: &str) -> bool {
                method == $method
            }
            fn method(&self) -> &str {
                $method
            }
            fn to_untyped_message(&self) -> Result<UntypedMessage, Error> {
                UntypedMessage::new($method, &self.0)
            }
            fn parse_message(method: &str, params: &impl Serialize) -> Result<Self, Error> {
                if !Self::matches_method(method) {
                    return Err(Error::method_not_found());
                }
                let value =
                    serde_json::to_value(params).map_err(|_| RemoteError::InvalidParams.wire())?;
                if !value.is_object() {
                    return Err(RemoteError::InvalidParams.wire());
                }
                Ok(Self(value))
            }
        }
        impl JsonRpcRequest for $name {
            type Response = $response;
        }
    };
}

raw_request!(Initialize, "initialize", Value);
raw_request!(NewSession, "session/new", v2::NewSessionResponse);
raw_request!(ListAgents, "_hive/agents/list", AgentsPage);

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ListParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

impl ListParams {
    pub fn validate(&self) -> Result<usize, Error> {
        if self
            .cursor
            .as_ref()
            .is_some_and(|cursor| cursor.is_empty() || cursor.len() > 128)
            || !(1..=100).contains(&self.limit.unwrap_or(100))
        {
            return Err(RemoteError::InvalidParams.wire());
        }
        Ok(self.limit.unwrap_or(100))
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, agent_client_protocol::JsonRpcResponse)]
#[serde(rename_all = "camelCase")]
pub struct AgentsPage {
    pub agents: Vec<AgentProfile>,
    pub default_agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentProfile {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub transport: String,
    pub versions: Vec<AgentVersion>,
    pub is_default: bool,
    pub capabilities: ProfileCapabilities,
    pub launch: LaunchRequirements,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ProfileCapabilities {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<v2::SessionCapabilities>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LaunchRequirements {
    pub cwd: String,
    pub workspace: String,
}

/// An initialized handle never survives reconnect and never retries creation.
pub struct Peer {
    connection: V2ConnectionTo<Agent>,
    pub node: String,
    selection: bool,
    agents: bool,
    workspaces: bool,
}

impl Peer {
    pub async fn initialize(
        connection: V2ConnectionTo<Agent>,
        mut request: v2::InitializeRequest,
    ) -> Result<Self, Error> {
        let meta = request.capabilities.meta.get_or_insert_default();
        meta.insert("hive".into(), json!({"versions": [1]}));
        let params =
            serde_json::to_value(request).map_err(|_| RemoteError::InvalidParams.wire())?;
        let response = connection
            .prepare_request(Initialize(params))
            .block_task()
            .await
            .map_err(|error| RemoteError::sanitized(error, RemoteError::Unsupported))?;
        let (node, selection, agents) = Self::validate(&response)?;
        let workspaces =
            response["capabilities"]["_meta"]["hive"]["features"]["workspaces"].is_object();
        Ok(Self {
            connection,
            node,
            selection,
            agents,
            workspaces,
        })
    }

    fn validate(response: &Value) -> Result<(String, bool, bool), Error> {
        if response["protocolVersion"] != 2 {
            return Err(RemoteError::Unsupported.wire());
        }
        let caps = &response["capabilities"];
        let hive = hive_metadata(caps)?.ok_or_else(|| RemoteError::Unsupported.wire())?;
        if hive["version"].as_u64() != Some(1) {
            return Err(RemoteError::InvalidParams.wire());
        }
        let node = hive["node"]
            .as_str()
            .filter(|node| hive_core::identity::NodeId::try_from((*node).to_owned()).is_ok())
            .ok_or_else(|| RemoteError::InvalidParams.wire())?
            .to_owned();
        let features = hive["features"]
            .as_object()
            .ok_or_else(|| RemoteError::InvalidParams.wire())?;
        for key in ["agents", "selection", "control", "location", "workspaces"] {
            if features.get(key).is_some_and(|value| {
                !value.is_null() && value.as_object().is_none_or(|object| !object.is_empty())
            }) {
                return Err(RemoteError::InvalidParams.wire());
            }
        }
        let enabled = |key| features.get(key).is_some_and(Value::is_object);
        if (enabled("selection") && (!enabled("agents") || !enabled("control")))
            || (enabled("workspaces") && !enabled("selection"))
            || (enabled("location") && !enabled("control"))
            || (["selection", "location", "control"]
                .iter()
                .any(|key| enabled(key))
                && !caps["session"].is_object())
        {
            return Err(RemoteError::InvalidParams.wire());
        }
        Ok((node, enabled("selection"), enabled("agents")))
    }

    pub async fn list(&self, params: ListParams) -> Result<AgentsPage, Error> {
        if !self.agents {
            return Err(RemoteError::Unsupported.wire());
        }
        params.validate()?;
        self.connection
            .prepare_request(ListAgents(
                serde_json::to_value(params).map_err(|_| RemoteError::InvalidParams.wire())?,
            ))
            .block_task()
            .await
            .map_err(|error| RemoteError::sanitized(error, RemoteError::Unsupported))
    }

    pub async fn start(
        &self,
        agent: &str,
        mut request: v2::NewSessionRequest,
    ) -> Result<v2::NewSessionResponse, Error> {
        if !self.selection {
            return Err(RemoteError::Unsupported.wire());
        }
        if agent.is_empty() {
            return Err(RemoteError::InvalidParams.wire());
        }
        if request
            .meta
            .as_ref()
            .and_then(|meta| meta.get("hive"))
            .is_some_and(|hive| hive.get("workspace").is_some())
            && !self.workspaces
        {
            return Err(RemoteError::Unsupported.wire());
        }
        let hive = request
            .meta
            .get_or_insert_default()
            .entry("hive")
            .or_insert_with(|| json!({}));
        hive.as_object_mut()
            .ok_or_else(|| RemoteError::InvalidParams.wire())?
            .insert("agent".into(), agent.into());
        self.connection
            .prepare_request(request)
            .block_task()
            .await
            .map_err(|error| RemoteError::sanitized(error, RemoteError::LaunchUncertain))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_metadata_and_versions_cannot_become_an_absent_offer() {
        for capabilities in [
            json!({"_meta":false}),
            json!({"_meta":{"hive":false}}),
            json!({"_meta":{"hive":{"versions":[]}}}),
            json!({"_meta":{"hive":{"versions":[1,1]}}}),
            json!({"_meta":{"hive":{"versions":[-1]}}}),
            json!({"_meta":{"hive":{"versions":[1.5]}}}),
        ] {
            assert!(offered_versions(&json!({"capabilities":capabilities})).is_err());
        }
        assert_eq!(offered_versions(&json!({"capabilities":{}})).unwrap(), None);
        assert_eq!(
            offered_versions(&json!({"capabilities":{"_meta":{"hive":null}}})).unwrap(),
            None
        );
        assert_eq!(
            offered_versions(&json!({"capabilities":{"_meta":{"hive":{"versions":[99,1]}}}}))
                .unwrap(),
            Some(vec![99, 1])
        );
    }

    #[test]
    fn recognized_peer_diagnostics_cannot_smuggle_private_messages_or_data() {
        let remote = Error::new(-32060, "PRIVATE_SENTINEL").data(json!({"code":"hive::remote::unknown_agent", "help":"PRIVATE_SENTINEL", "env":"PRIVATE_SENTINEL"}));
        let controlled = RemoteError::sanitized(remote, RemoteError::LaunchUncertain);
        let value = serde_json::to_value(controlled).unwrap();
        assert_eq!(value["code"], -32602);
        assert_eq!(value["data"]["code"], "hive::remote::unknown_agent");
        assert!(!value.to_string().contains("PRIVATE_SENTINEL"));
        assert!(
            !format!("{:?}", NewSession(json!({"_meta":"PRIVATE_SENTINEL"})))
                .contains("PRIVATE_SENTINEL")
        );
        for kind in [
            RemoteError::ProjectConflict,
            RemoteError::ProvisioningFailed,
            RemoteError::ProvisioningUncertain,
        ] {
            let error = Error::new(-32060, "PRIVATE_SENTINEL")
                .data(json!({"code":kind.code(),"stderr":"PRIVATE_SENTINEL"}));
            let controlled = RemoteError::sanitized(error, RemoteError::LaunchUncertain);
            let value = serde_json::to_value(controlled).unwrap();
            assert_eq!(value["data"]["code"], kind.code());
            assert!(!value.to_string().contains("PRIVATE_SENTINEL"));
        }
    }

    #[test]
    fn unoffered_versions_invalid_markers_and_missing_dependencies_disable_peer_operations() {
        let node = hive_core::identity::NodeId::new().to_string();
        let valid = json!({"protocolVersion":2,"capabilities":{"session":{},"_meta":{"hive":advertisement(&node)}}});
        assert_eq!(Peer::validate(&valid).unwrap(), (node, true, true));
        for (pointer, value) in [
            ("/capabilities/_meta/hive/version", json!(99)),
            (
                "/capabilities/_meta/hive/node",
                json!("http://PRIVATE_SENTINEL"),
            ),
            ("/capabilities/_meta/hive/features/agents", json!(true)),
            ("/capabilities/_meta/hive/features/workspaces", json!(true)),
            ("/capabilities/_meta/hive/features/selection", Value::Null),
            ("/capabilities/_meta/hive/features/control", Value::Null),
            ("/capabilities/_meta/hive/features/agents", Value::Null),
            ("/capabilities/session", Value::Null),
            ("/capabilities/_meta", json!(false)),
        ] {
            let mut invalid = valid.clone();
            *invalid.pointer_mut(pointer).unwrap() = value;
            assert!(
                Peer::validate(&invalid).is_err(),
                "accepted invalid {pointer}"
            );
        }
        let mut additive = valid;
        additive["capabilities"]["_meta"]["hive"]["features"]["future"] = json!(true);
        assert!(Peer::validate(&additive).is_ok());
        assert!(
            Peer::validate(&json!({"protocolVersion":2,"capabilities":{"session":{}}})).is_err()
        );
    }
}
