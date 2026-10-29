//! Portable identifiers: none encodes a path, display name, or configuration source.

use serde::{Deserialize, Serialize};

/// An identifier was malformed or belonged to a different resource kind.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("invalid {kind} identifier; use a stored {prefix}<UUID> identifier")]
pub struct InvalidId {
    pub kind: &'static str,
    pub prefix: &'static str,
}

impl InvalidId {
    /// Stable identifier for binary-edge diagnostic adapters.
    #[must_use]
    pub fn code(&self) -> &'static str {
        "hive::identity::invalid_id"
    }

    #[must_use]
    pub fn help(&self) -> &'static str {
        "Use the stored identifier for the requested resource kind; do not derive one from a path or display name."
    }
}

macro_rules! identity {
    ($name:ident, $prefix:literal, $description:literal) => {
        #[doc = $description]
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            /// Allocate once at resource registration, then persist and reuse.
            #[must_use]
            pub fn new() -> Self {
                Self(format!("{}{}", $prefix, uuid::Uuid::new_v4()))
            }

            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl TryFrom<String> for $name {
            type Error = InvalidId;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                let valid = value.strip_prefix($prefix).is_some_and(|suffix| {
                    // Canonical spellings prevent two strings identifying the same UUID.
                    uuid::Uuid::parse_str(suffix)
                        .is_ok_and(|id| id.hyphenated().to_string() == suffix)
                });
                if valid {
                    Ok(Self(value))
                } else {
                    Err(InvalidId {
                        kind: stringify!($name),
                        prefix: $prefix,
                    })
                }
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

identity!(ProjectId, "project-", "Stable logical project identity.");
identity!(
    NodeId,
    "node-",
    "Execution-node identity, allocated once by its registry."
);
identity!(
    RepositoryId,
    "repository-",
    "Identity of a repository resource on one node."
);
identity!(
    WorkspaceId,
    "workspace-",
    "Identity of a concrete working environment."
);
identity!(
    OperationId,
    "operation-",
    "Durable key for one logical workspace mutation."
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_round_trip_without_paths_and_reject_other_resource_kinds() {
        let id = ProjectId::new();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(serde_json::from_str::<ProjectId>(&json).unwrap(), id);
        assert!(serde_json::from_str::<WorkspaceId>(&json).is_err());
        let invalid = ProjectId::try_from("project-not-a-uuid".to_owned()).unwrap_err();
        assert_eq!(invalid.code(), "hive::identity::invalid_id");
        assert!(invalid.help().contains("stored identifier"));
        assert_ne!(id, ProjectId::new());
        for malformed in [
            "",
            "/tmp/project",
            "project-00000000000000000000000000000000",
        ] {
            assert!(ProjectId::try_from(malformed.to_owned()).is_err());
        }
    }
}
