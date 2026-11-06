//! Hive-only launch selectors are consumed by the gateway, never downstream agents.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Launch {
    New {
        project: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        definition: Option<hive_core::project::ProjectDefinition>,
        policy: Option<String>,
        #[serde(rename = "baseRef")]
        base_ref: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        revision: Option<String>,
        branch: Option<String>,
    },
    Attach {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        owner: Option<hive_core::identity::NodeId>,
    },
    Existing {
        directory: String,
    },
}

impl Launch {
    /// A definition must never acquire an identity implicitly from a remote profile default.
    pub fn validate(&self) -> Result<(), crate::remote::RemoteError> {
        if let Self::New {
            project,
            definition,
            ..
        } = self
            && (definition.is_some() && project.is_none()
                || project.as_ref().is_some_and(|id| {
                    hive_core::identity::ProjectId::try_from(id.clone()).is_err()
                }))
        {
            return Err(crate::remote::RemoteError::InvalidParams);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Context {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<hive_core::identity::NodeId>,
    pub project: String,
    pub workspace: String,
    pub effective_cwd: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn portable_definitions_require_explicit_identity_and_cannot_import_node_local_configuration() {
        let mut value = json!({"kind":"new","definition":{"name":"Demo","repository_source":"https://example.test/demo.git"}});
        assert!(
            serde_json::from_value::<Launch>(value.clone())
                .unwrap()
                .validate()
                .is_err()
        );
        value["project"] = json!("demo");
        assert!(
            serde_json::from_value::<Launch>(value.clone())
                .unwrap()
                .validate()
                .is_ok()
        );
        for field in ["directory", "credentials", "env"] {
            let mut invalid = value.clone();
            invalid["definition"][field] = json!("PRIVATE_SENTINEL");
            assert!(serde_json::from_value::<Launch>(invalid).is_err());
        }
        value["definition"]["repository_source"] = json!("https://user:secret@example.test/repo");
        assert!(serde_json::from_value::<Launch>(value).is_err());
    }

    #[test]
    fn persisted_workspace_contexts_without_an_owner_remain_readable() {
        let context: Context = serde_json::from_value(
            json!({"project":"demo","workspace":"workspace-old","effectiveCwd":"/owner/path"}),
        )
        .unwrap();
        assert!(context.owner.is_none());
        assert_eq!(context.effective_cwd, "/owner/path");
    }
}
