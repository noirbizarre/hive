//! Logical projects and reusable node-local repositories have separate lifetimes.

use serde::{Deserialize, Serialize};

use crate::{
    identity::{NodeId, ProjectId, RepositoryId},
    workspace::{NodePath, Ownership},
};

/// Logical project metadata; renaming or relocating a checkout preserves `id`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub id: ProjectId,
    pub name: String,
    /// An opaque source locator, not a node-local path or embedded credential store.
    /// Backends validate source syntax and resolve authentication on the owning node.
    pub repository_source: Option<String>,
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
    fn relocating_checkouts_and_renaming_projects_preserves_portable_identity() {
        let mut project = Project {
            id: ProjectId::new(),
            name: "original".to_owned(),
            repository_source: None,
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
        project.name = "renamed".to_owned();
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
