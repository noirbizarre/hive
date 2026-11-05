//! Logical projects and reusable node-local repositories have separate lifetimes.

use serde::{Deserialize, Serialize};

/// SCM selection is independent of how a task workspace is provisioned.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scm {
    #[default]
    Git,
    Jj,
}
impl Scm {
    fn is_git(&self) -> bool {
        *self == Self::Git
    }
}

/// JJ expressions are not Git refs: revsets and change IDs remain intact.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Revision(String);

impl TryFrom<String> for Revision {
    type Error = ProjectError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.trim().is_empty() || value.chars().any(char::is_control) {
            return Err(ProjectError::Revision);
        }
        Ok(Self(value))
    }
}
impl From<Revision> for String {
    fn from(value: Revision) -> Self {
        value.0
    }
}
impl Revision {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

use crate::{
    identity::{NodeId, ProjectId, RepositoryId},
    workspace::{GitRef, NodePath, Ownership},
};

/// Logical project metadata; renaming or relocating a checkout preserves `id`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub id: ProjectId,
    #[serde(flatten)]
    pub definition: ProjectDefinition,
}

/// Portable intent only: local paths, credentials and checkout observations live elsewhere.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "UncheckedDefinition")]
pub struct ProjectDefinition {
    pub name: String,
    #[serde(skip_serializing_if = "Scm::is_git")]
    pub scm: Scm,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_revision: Option<Revision>,
    pub repository_source: Option<String>,
    pub default_ref: Option<GitRef>,
    pub preparation: Vec<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UncheckedDefinition {
    name: String,
    #[serde(default)]
    scm: Scm,
    default_revision: Option<Revision>,
    repository_source: Option<String>,
    default_ref: Option<GitRef>,
    #[serde(default)]
    preparation: Vec<Vec<String>>,
}

impl TryFrom<UncheckedDefinition> for ProjectDefinition {
    type Error = ProjectError;

    fn try_from(value: UncheckedDefinition) -> Result<Self, Self::Error> {
        let definition = Self {
            name: value.name,
            scm: value.scm,
            default_revision: value.default_revision,
            repository_source: value.repository_source,
            default_ref: value.default_ref,
            preparation: value.preparation,
        };
        definition.validate()?;
        Ok(definition)
    }
}

/// Controlled reasons prevent diagnostics from copying remote tokens or command arguments.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ProjectError {
    #[error(
        "invalid SCM revision configuration; use default_ref for Git or default_revision with a nonempty JJ revset for JJ"
    )]
    Revision,
    #[error("project name must not be blank or contain control characters")]
    Name,
    #[error("project remote must be a credential-free HTTPS or SSH Git locator")]
    Remote,
    #[error("project preparation requires nonempty argv commands without NUL characters")]
    Preparation,
}

impl ProjectError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Revision => "hive::project::revision",
            Self::Name => "hive::project::name",
            Self::Remote => "hive::project::remote",
            Self::Preparation => "hive::project::preparation",
        }
    }

    pub fn help(&self) -> &'static str {
        match self {
            Self::Revision => {
                "select scm = 'jj' for default_revision; do not mix Git refs and JJ revsets"
            }
            Self::Name => "give the project a nonblank display name",
            Self::Remote => {
                "use https://host/repo, ssh://user@host/repo or user@host:repo; configure credentials on the execution node"
            }
            Self::Preparation => {
                "provide argv arrays with a nonblank executable; preparation is stored, not executed"
            }
        }
    }
}

impl ProjectDefinition {
    pub fn validate(&self) -> Result<(), ProjectError> {
        if (self.scm == Scm::Git && self.default_revision.is_some())
            || (self.scm == Scm::Jj && self.default_ref.is_some())
        {
            return Err(ProjectError::Revision);
        }
        if self.name.trim().is_empty() || self.name.chars().any(char::is_control) {
            return Err(ProjectError::Name);
        }
        if let Some(source) = &self.repository_source {
            validate_remote(source)?;
        }
        if self.preparation.iter().any(|argv| {
            argv.first()
                .is_none_or(|executable| executable.trim().is_empty())
                || argv.iter().any(|arg| arg.contains('\0'))
        }) {
            return Err(ProjectError::Preparation);
        }
        Ok(())
    }
}

fn validate_remote(source: &str) -> Result<(), ProjectError> {
    if source.is_empty()
        || source.chars().any(|c| c.is_whitespace() || c.is_control())
        || source.contains(['?', '#', '\\'])
    {
        return Err(ProjectError::Remote);
    }
    if source.contains("://") {
        let remote = url::Url::parse(source).map_err(|_| ProjectError::Remote)?;
        if !matches!(remote.scheme(), "https" | "ssh")
            || remote.host_str().is_none()
            || remote.password().is_some()
            || (remote.scheme() == "https" && !remote.username().is_empty())
            || remote.path().trim_matches('/').is_empty()
        {
            return Err(ProjectError::Remote);
        }
    } else {
        // Requiring an SSH user disambiguates SCP syntax from local paths and drive letters.
        let (authority, path) = source.split_once(':').ok_or(ProjectError::Remote)?;
        let (user, host) = authority.split_once('@').ok_or(ProjectError::Remote)?;
        if user.is_empty()
            || host.is_empty()
            || path.is_empty()
            || authority.contains('/')
            || host.contains('@')
            || path.starts_with('-')
        {
            return Err(ProjectError::Remote);
        }
        let probe = url::Url::parse(&format!("ssh://{authority}/repo"))
            .map_err(|_| ProjectError::Remote)?;
        if probe.host_str().is_none() || probe.password().is_some() {
            return Err(ProjectError::Remote);
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepositoryRole {
    Checkout,
    Cache,
}

/// A reusable checkout/cache, not a task workspace and not owned by a session.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct NodeRepository {
    pub id: RepositoryId,
    pub project: ProjectId,
    /// The root carries node affinity, avoiding contradictory node/path fields.
    pub root: NodePath,
    pub role: RepositoryRole,
    pub ownership: Ownership,
}

impl NodeRepository {
    #[must_use]
    pub fn node(&self) -> &NodeId {
        self.root.node()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::PathSyntax;

    #[test]
    fn jj_revsets_remain_native_while_legacy_projects_keep_their_git_defaults() {
        let legacy: ProjectDefinition =
            serde_json::from_value(serde_json::json!({"name":"old", "default_ref":"main"}))
                .unwrap();
        assert_eq!(legacy.scm, Scm::Git);
        let serialized = serde_json::to_value(legacy).unwrap();
        assert!(serialized.get("scm").is_none());
        assert!(serialized.get("default_revision").is_none());
        let jj: ProjectDefinition = serde_json::from_value(serde_json::json!({"name":"JJ", "scm":"jj", "default_revision":"latest(trunk() | mine())"})).unwrap();
        assert_eq!(
            jj.default_revision.unwrap().as_str(),
            "latest(trunk() | mine())"
        );
        for invalid in [
            serde_json::json!({"name":"mixed", "scm":"jj", "default_ref":"main"}),
            serde_json::json!({"name":"mixed", "default_revision":"@"}),
            serde_json::json!({"name":"empty", "scm":"jj", "default_revision":" "}),
            serde_json::json!({"name":"control", "scm":"jj", "default_revision":"@\n"}),
        ] {
            assert!(serde_json::from_value::<ProjectDefinition>(invalid).is_err());
        }
    }

    #[test]
    fn portable_definitions_accept_ssh_and_https_without_contacting_a_repository() {
        for source in [
            "https://example.test/team/repo.git",
            "ssh://git@example.test:2222/repo",
            "git@example.test:team/repo.git",
        ] {
            let definition: ProjectDefinition = serde_json::from_value(serde_json::json!({
                "name":"Repository", "repository_source":source, "default_ref":"main",
                "preparation":[["tool", "an argument with spaces"]]
            }))
            .unwrap();
            definition.validate().unwrap();
        }
        let local: ProjectDefinition =
            serde_json::from_value(serde_json::json!({"name":"Local only"})).unwrap();
        assert!(local.repository_source.is_none());
    }

    #[test]
    fn invalid_remotes_and_credentials_are_rejected_without_echoing_values() {
        for source in [
            "",
            "/local/repo",
            "../repo",
            "file:///tmp/repo",
            "http://example.test/repo",
            "https://example.test",
            "https://user:secret@example.test/repo",
            "https://secret@example.test/repo",
            "ssh://git:secret@example.test/repo",
            "https://example.test/repo?token=secret",
            "git@:repo",
            "git@example.test:",
            "git@example.test:repo\n",
        ] {
            let error = validate_remote(source).unwrap_err();
            assert_eq!(error, ProjectError::Remote);
            assert!(!error.to_string().contains("secret"));
        }
    }

    #[test]
    fn missing_names_invalid_refs_and_invalid_preparation_never_decode() {
        for value in [
            serde_json::json!({}),
            serde_json::json!({"name":" "}),
            serde_json::json!({"name":"ok", "default_ref":"--bad"}),
            serde_json::json!({"name":"ok", "preparation":[[]]}),
            serde_json::json!({"name":"ok", "preparation":[[" "]]}),
            serde_json::json!({"name":"ok", "preparation":[["tool", "\0"]]}),
            serde_json::json!({"name":"ok", "directory":"/not/portable"}),
        ] {
            assert!(serde_json::from_value::<ProjectDefinition>(value).is_err());
        }
    }

    #[test]
    fn relocating_checkouts_and_renaming_projects_preserves_portable_identity() {
        let mut project = Project {
            id: ProjectId::new(),
            definition: ProjectDefinition {
                scm: Scm::Git,
                default_revision: None,
                name: "original".to_owned(),
                repository_source: None,
                default_ref: None,
                preparation: Vec::new(),
            },
        };
        let original = project.id.clone();
        let node = NodeId::new();
        let mut repository = NodeRepository {
            id: RepositoryId::new(),
            project: original.clone(),
            root: NodePath::new(node.clone(), PathSyntax::Posix, "/cache/one".to_owned()).unwrap(),
            role: RepositoryRole::Cache,
            ownership: Ownership::HiveManaged,
        };
        let repository_id = repository.id.clone();
        project.definition.name = "renamed".to_owned();
        repository.root =
            NodePath::new(node.clone(), PathSyntax::Posix, "/cache/two".to_owned()).unwrap();
        assert_eq!(project.id, original);
        assert_eq!(repository.id, repository_id);
        assert_eq!(repository.project, original);
        assert_eq!(repository.node(), &node);
        let json = serde_json::to_string(&repository).unwrap();
        assert_eq!(
            serde_json::from_str::<NodeRepository>(&json).unwrap(),
            repository
        );
    }
}
