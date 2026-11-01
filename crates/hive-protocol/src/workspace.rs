//! Hive-only launch selectors are consumed by the gateway, never downstream agents.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Launch {
    New {
        project: Option<String>,
        policy: Option<String>,
        #[serde(rename = "baseRef")]
        base_ref: Option<String>,
        branch: Option<String>,
    },
    Attach {
        id: String,
    },
    Existing {
        directory: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Context {
    pub project: String,
    pub workspace: String,
    pub effective_cwd: String,
}
