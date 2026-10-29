//! Portable identifiers: allocated once, independent of paths and configuration sources.

use serde::{Deserialize, Serialize};

/// An identifier was malformed or belonged to a different resource kind.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("invalid {kind} identifier; use a stored identifier for this resource kind")]
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
        "Use the stored identifier for the requested resource kind; project IDs accept lowercase slugs, while other resource IDs require their prefix and canonical UUID."
    }
}

macro_rules! identity {
    ($name:ident, $prefix:literal, $description:literal) => {
        identity!($name, $prefix, $description, |value: &str| uuid_id(
            value, $prefix
        ));
    };
    ($name:ident, $prefix:literal, $description:literal, $validate:expr) => {
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
                let valid = ($validate)(&value);
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

fn uuid_id(value: &str, prefix: &str) -> bool {
    value.strip_prefix(prefix).is_some_and(|suffix| {
        // Canonical spellings prevent two strings identifying the same UUID.
        uuid::Uuid::parse_str(suffix).is_ok_and(|id| id.hyphenated().to_string() == suffix)
    })
}

fn project_id(value: &str) -> bool {
    let slug = !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && !value.starts_with('-')
        && !value.ends_with('-')
        && !value.contains("--");
    // Accept existing project UUIDs, but do not reinterpret another resource's stored ID as a project.
    slug && !["node-", "repository-", "workspace-", "operation-"]
        .iter()
        .any(|prefix| uuid_id(value, prefix))
}

identity!(
    ProjectId,
    "project-",
    "Stable logical project identity.",
    project_id
);

impl ProjectId {
    /// Initial name-derived candidate; the registry assigns a suffix if it is already taken.
    /// Persist the chosen ID rather than regenerating it when the display name changes.
    #[must_use]
    pub fn from_name(name: &str) -> Self {
        let mut slug = String::new();
        for character in name.chars() {
            if character.is_ascii_alphanumeric() {
                slug.push(character.to_ascii_lowercase());
            } else if !slug.is_empty() && !slug.ends_with('-') {
                slug.push('-');
            }
        }
        slug.truncate(slug.trim_end_matches('-').len());
        if slug.is_empty() {
            // Names without ASCII letters or digits still need a usable, deterministic candidate.
            slug.push_str("project");
        }
        if !project_id(&slug) {
            slug.insert_str(0, "project-");
        }
        Self(slug)
    }
}
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
        let invalid = ProjectId::try_from("Not a slug".to_owned()).unwrap_err();
        assert_eq!(invalid.code(), "hive::identity::invalid_id");
        assert!(invalid.help().contains("stored identifier"));
        assert_ne!(id, ProjectId::new());
        for malformed in [
            "",
            "/tmp/project",
            "My-Project",
            "my--project",
            "-project",
            "project-",
        ] {
            assert!(ProjectId::try_from(malformed.to_owned()).is_err());
        }
        assert!(ProjectId::try_from(NodeId::new().to_string()).is_err());
    }

    #[test]
    fn project_names_produce_lowercase_slug_candidates_without_changing_existing_ids() {
        for (name, slug) in [
            ("Hive", "hive"),
            ("  My Cool_Project!  ", "my-cool-project"),
            ("Tools / CLI", "tools-cli"),
            ("123", "123"),
            ("日本語", "project"),
        ] {
            let id = ProjectId::from_name(name);
            assert_eq!(id.as_str(), slug);
            assert_eq!(ProjectId::try_from(slug.to_owned()).unwrap(), id);
            assert_eq!(
                serde_json::from_str::<ProjectId>(&serde_json::to_string(&id).unwrap()).unwrap(),
                id
            );
        }
        let legacy = "project-11111111-1111-4111-8111-111111111111";
        assert_eq!(
            ProjectId::try_from(legacy.to_owned()).unwrap().as_str(),
            legacy
        );
    }
}
