//! Named working setups compose by identity, never by source filename.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use hive_core::{
    identity::ProjectId,
    workspace::{GitRef, Retention},
};
use serde::{Deserialize, Serialize};

use super::{Config, ConfigError, WorkspaceTemplate};

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionProfile {
    pub name: Option<String>,
    #[serde(default)]
    pub agents: BTreeSet<String>,
    #[serde(default)]
    pub projects: BTreeSet<ProjectId>,
    pub default_agent: Option<String>,
    pub default_project: Option<ProjectId>,
    #[serde(default)]
    pub workspaces: BTreeMap<ProjectId, WorkspaceDeclaration>,
}

// Preserve field presence until composition: a discriminator may live in another fragment.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceDeclaration {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_ref: Option<GitRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<hive_core::project::Revision>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub directory: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backing: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retention: Option<Retention>,
}

impl WorkspaceDeclaration {
    pub fn resolve(&self, config: &Config) -> Result<WorkspaceTemplate, String> {
        let mut fields = serde_json::to_value(self).expect("workspace fields are serializable");
        if let Some(template) = &self.template {
            fields
                .as_object_mut()
                .expect("workspace object")
                .remove("template");
            if !fields.as_object().expect("workspace object").is_empty() {
                return Err("template references cannot also declare inline policy fields".into());
            }
            return config.workspaces.get(template).cloned().ok_or_else(|| {
                format!("unknown workspace template '{template}'; declare it under [workspaces]")
            });
        }
        // Reuse the strict policy schema without exposing parser values in diagnostics.
        serde_json::from_value(fields).map_err(|_| {
            "invalid inline policy; supply policy and only settings supported by that policy".into()
        })
    }
}

type Origins = BTreeMap<(String, String), PathBuf>;

fn scalar<T: PartialEq>(
    target: &mut Option<T>,
    value: Option<T>,
    id: &str,
    field: &str,
    path: &Path,
    origins: &mut Origins,
) -> Result<(), ConfigError> {
    if let Some(value) = value {
        let key = (id.to_owned(), field.to_owned());
        if target.as_ref().is_some_and(|previous| previous != &value) {
            return Err(ConfigError::ProfileConflict(format!(
                "profile '{id}' field '{field}' conflicts between {} and {}",
                origins
                    .get(&key)
                    .expect("declared field has an origin")
                    .display(),
                path.display()
            )));
        }
        origins.entry(key).or_insert_with(|| path.to_path_buf());
        *target = Some(value);
    }
    Ok(())
}

pub(super) fn merge(
    target: &mut BTreeMap<String, ExecutionProfile>,
    incoming: BTreeMap<String, ExecutionProfile>,
    path: &Path,
    origins: &mut Origins,
) -> Result<(), ConfigError> {
    for (id, incoming) in incoming {
        let profile = target.entry(id.clone()).or_default();
        profile.agents.extend(incoming.agents);
        profile.projects.extend(incoming.projects);
        scalar(&mut profile.name, incoming.name, &id, "name", path, origins)?;
        scalar(
            &mut profile.default_agent,
            incoming.default_agent,
            &id,
            "default_agent",
            path,
            origins,
        )?;
        scalar(
            &mut profile.default_project,
            incoming.default_project,
            &id,
            "default_project",
            path,
            origins,
        )?;
        for (project, incoming) in incoming.workspaces {
            let workspace = profile.workspaces.entry(project.clone()).or_default();
            macro_rules! merge_fields {
                ($($field:ident),+) => {
                    $(scalar(&mut workspace.$field, incoming.$field, &id,
                        &format!("workspaces.{project}.{}", stringify!($field)), path, origins)?;)+
                };
            }
            merge_fields!(
                template, policy, base_ref, revision, directory, backing, retention
            );
        }
    }
    Ok(())
}

impl Config {
    pub(super) fn execution_problems(&self) -> Vec<ConfigError> {
        let mut errors = Vec::new();
        if let Some(default) = &self.default_profile
            && !self.profiles.contains_key(default)
        {
            errors.push(ConfigError::ExecutionProfile(format!(
                "default_profile '{default}' is not declared under [profiles]"
            )));
        }
        for (id, profile) in &self.profiles {
            let mut problem = |message: String| {
                errors.push(ConfigError::ExecutionProfile(format!(
                    "profile '{id}': {message}"
                )))
            };
            if id.is_empty()
                || !id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
            {
                problem("ID must use letters, digits, '-' or '_'".into());
            }
            if profile.agents.is_empty() {
                problem("declare at least one agent in agents".into());
            }
            for agent in &profile.agents {
                if !self.agents.contains_key(agent) {
                    problem(format!(
                        "unknown agent '{agent}'; declare it under [agents]"
                    ));
                }
            }
            for project in &profile.projects {
                if !self.projects.contains_key(project) {
                    problem(format!(
                        "unknown project '{project}'; declare it under [projects]"
                    ));
                }
            }
            if profile
                .default_agent
                .as_ref()
                .is_some_and(|id| !profile.agents.contains(id))
            {
                problem("default_agent must belong to agents".into());
            }
            if profile
                .default_project
                .as_ref()
                .is_some_and(|id| !profile.projects.contains(id))
            {
                problem("default_project must belong to projects".into());
            }
            for (project, declaration) in &profile.workspaces {
                if !profile.projects.contains(project) {
                    problem(format!(
                        "workspace project '{project}' must belong to projects"
                    ));
                }
                match declaration.resolve(self) {
                    Err(error) => problem(format!("workspace for '{project}': {error}")),
                    Ok(policy) => {
                        if let Err(error) = policy.validate() {
                            problem(format!("workspace for '{project}': {error}"));
                        }
                        if self
                            .projects
                            .get(project)
                            .is_some_and(|definition| !policy.supports_scm(definition.scm))
                        {
                            problem(format!(
                                "workspace for '{project}' is incompatible with its project SCM; select a compatible policy"
                            ));
                        }
                    }
                }
            }
        }
        errors
    }

    /// Resolve membership and defaults once, while admission holds its config snapshot.
    pub fn select_execution<'a>(
        &'a self,
        requested: Option<&str>,
        agent: Option<String>,
    ) -> Result<(String, Option<&'a ExecutionProfile>), ConfigError> {
        let selected = requested.or(self.default_profile.as_deref());
        let Some(id) = selected else {
            return agent
                .or_else(|| self.default_agent.clone())
                .map(|agent| (agent, None))
                .ok_or_else(|| {
                    ConfigError::DefaultAgent(
                        "no default agent configured; set default_agent or pass _meta.hive.agent"
                            .into(),
                    )
                });
        };
        let fail =
            |message: &str| ConfigError::ExecutionProfile(format!("profile '{id}': {message}"));
        let profile = self
            .profiles
            .get(id)
            .ok_or_else(|| fail("unknown profile; refresh GET /v1/providers"))?;
        if agent
            .as_ref()
            .is_some_and(|agent| !profile.agents.contains(agent))
        {
            return Err(fail("requested agent is outside agents; select a member"));
        }
        let agent = agent
            .or_else(|| profile.default_agent.clone())
            .or_else(|| {
                self.default_agent
                    .clone()
                    .filter(|id| profile.agents.contains(id))
            })
            .or_else(|| {
                (profile.agents.len() == 1)
                    .then(|| profile.agents.first().expect("singleton").clone())
            })
            .ok_or_else(|| {
                fail("agent selection is ambiguous; set default_agent or pass _meta.hive.agent")
            })?;
        Ok((agent, Some(profile)))
    }

    pub fn public_execution_profiles(&self) -> Vec<hive_protocol::remote::ExecutionProfile> {
        self.profiles
            .iter()
            .map(|(id, profile)| hive_protocol::remote::ExecutionProfile {
                id: id.clone(),
                name: profile.name.clone().unwrap_or_else(|| id.clone()),
                agents: profile.agents.iter().cloned().collect(),
                projects: profile.projects.iter().map(ToString::to_string).collect(),
                default_agent: profile.default_agent.clone(),
                default_project: profile.default_project.as_ref().map(ToString::to_string),
                is_default: self.default_profile.as_ref() == Some(id),
                workspaces: profile
                    .workspaces
                    .iter()
                    .map(|(project, declaration)| {
                        (
                            project.to_string(),
                            if let Some(template) = &declaration.template {
                                serde_json::json!({"template": template})
                            } else {
                                serde_json::json!({"policy": declaration.policy})
                            },
                        )
                    })
                    .collect(),
            })
            .collect()
    }
}

impl ExecutionProfile {
    pub(crate) fn automatic_workspace(
        &self,
        legacy_project: Option<ProjectId>,
        legacy_policy: bool,
    ) -> Result<bool, ConfigError> {
        let project = self.project(None, legacy_project)?;
        if project.is_none() && (legacy_policy || !self.workspaces.is_empty()) {
            return Err(ConfigError::ExecutionProfile(
                "project selection is ambiguous; set default_project or pass workspace.project"
                    .into(),
            ));
        }
        Ok(legacy_policy
            || project
                .as_ref()
                .is_some_and(|project| self.workspaces.contains_key(project)))
    }

    pub fn project(
        &self,
        explicit: Option<ProjectId>,
        legacy: Option<ProjectId>,
    ) -> Result<Option<ProjectId>, ConfigError> {
        if explicit
            .as_ref()
            .is_some_and(|id| !self.projects.contains(id))
        {
            return Err(ConfigError::ExecutionProfile(
                "requested project is outside the selected profile; select a member project".into(),
            ));
        }
        Ok(explicit
            .or_else(|| self.default_project.clone())
            .or_else(|| legacy.filter(|id| self.projects.contains(id)))
            .or_else(|| {
                (self.projects.len() == 1)
                    .then(|| self.projects.first().expect("singleton").clone())
            }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ConfigSource;

    const ENTITIES: &str = "[agents.a]\ncommand = 'a'\nproject = 'one'\nworkspace = 'task'\n[agents.b]\ncommand = 'b'\n[projects.one]\nname = 'One'\n[projects.two]\nname = 'Two'\n[workspaces.task]\npolicy = 'worktree'\n";

    #[test]
    fn named_profiles_compose_members_and_nested_policies_before_resolving_forward_references() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("config.toml");
        std::fs::write(dir.path().join("entities.toml"), ENTITIES).unwrap();
        std::fs::write(dir.path().join("members.toml"), "[profiles.personal]\nagents = ['b','a','a']\nprojects = ['one','two']\n[profiles.personal.workspaces.one]\npolicy = 'worktree'\n").unwrap();
        std::fs::write(dir.path().join("settings.toml"), "[profiles.personal]\nagents = ['a']\ndefault_agent = 'b'\n[profiles.personal.workspaces.one]\nbase_ref = 'main'\n[profiles.personal.workspaces.two]\ntemplate = 'task'\n").unwrap();
        for includes in [
            "['members.toml','settings.toml','entities.toml']",
            "['entities.toml','settings.toml','members.toml']",
        ] {
            std::fs::write(
                &root,
                format!("default_profile = 'personal'\ninclude = {includes}\n"),
            )
            .unwrap();
            let config = ConfigSource::new(Some(root.clone()))
                .unwrap()
                .load()
                .unwrap();
            let profile = &config.profiles["personal"];
            assert_eq!(
                profile
                    .agents
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
                ["a", "b"]
            );
            assert_eq!(config.select_execution(None, None).unwrap().0, "b");
            assert_eq!(
                profile.workspaces[&ProjectId::try_from("one".to_owned()).unwrap()]
                    .resolve(&config)
                    .unwrap(),
                WorkspaceTemplate::Worktree {
                    base_ref: Some(GitRef::try_from("main".to_owned()).unwrap())
                }
            );
            assert_eq!(
                profile.workspaces[&ProjectId::try_from("two".to_owned()).unwrap()]
                    .resolve(&config)
                    .unwrap(),
                config.workspaces["task"]
            );
        }
        std::fs::rename(
            dir.path().join("members.toml"),
            dir.path().join("moved.toml"),
        )
        .unwrap();
        std::fs::write(
            &root,
            "include = ['moved.toml','settings.toml','entities.toml']",
        )
        .unwrap();
        assert!(
            ConfigSource::new(Some(root))
                .unwrap()
                .load()
                .unwrap()
                .profiles
                .contains_key("personal")
        );
    }

    #[test]
    fn conflicting_nested_profile_fields_report_both_sources_without_values() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("config.toml");
        std::fs::write(&root, "include = ['fragment.toml']\n[profiles.p.workspaces.one]\nbase_ref = 'private-first'\n").unwrap();
        let fragment = dir.path().join("fragment.toml");
        std::fs::write(
            &fragment,
            "[profiles.p.workspaces.one]\nbase_ref = 'private-second'\n",
        )
        .unwrap();
        let error = ConfigSource::new(Some(root.clone()))
            .unwrap()
            .load()
            .unwrap_err();
        assert!(matches!(error, ConfigError::ProfileConflict(_)));
        let message = error.to_string();
        for text in [
            "profile 'p'",
            "workspaces.one.base_ref",
            root.to_str().unwrap(),
            fragment.to_str().unwrap(),
        ] {
            assert!(message.contains(text));
        }
        assert!(!message.contains("private-"));
        std::fs::write(
            &fragment,
            "[profiles.p.workspaces.one]\nbase_ref = 'private-first'\n",
        )
        .unwrap();
        assert!(Config::read(Some(root)).is_ok());
    }

    #[test]
    fn invalid_named_profiles_are_diagnosed_after_composition() {
        for declaration in [
            "[profiles.p]\nagents = ['missing']",
            "[profiles.p]\nagents = ['a']\nprojects = ['missing']",
            "[profiles.p]\nagents = ['a']\ndefault_agent = 'b'",
            "[profiles.p]\nagents = ['a']\ndefault_project = 'one'",
            "[profiles.p]\nagents = ['a']\n[profiles.p.workspaces.one]\npolicy = 'worktree'",
            "[profiles.p]\nagents = ['a']\nprojects = ['one']\n[profiles.p.workspaces.one]\ntemplate = 'missing'",
            "[profiles.p]\nagents = ['a']\nprojects = ['one']\n[profiles.p.workspaces.one]\ntemplate = 'task'\npolicy = 'worktree'",
            "[profiles.p]\nagents = ['a']\nprojects = ['one']\n[profiles.p.workspaces.one]\npolicy = 'jj'",
            "[profiles.p]\nagents = ['a']\nprojects = ['one']\n[profiles.p.workspaces.one]\nbase_ref = 'main'",
            "[profiles.'bad.id']\nagents = ['a']",
            "[profiles.empty]",
        ] {
            let config: Config = toml::from_str(&format!("{ENTITIES}\n{declaration}")).unwrap();
            assert!(
                config
                    .problems()
                    .iter()
                    .any(|problem| problem.agent.is_none()
                        && matches!(problem.error, ConfigError::ExecutionProfile(_))),
                "{declaration}"
            );
        }
        let config: Config = toml::from_str("default_profile = 'missing'").unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn selection_inherits_only_member_defaults_and_requires_unambiguous_choices() {
        let mut config: Config = toml::from_str(&format!("default_agent = 'b'\n{ENTITIES}\n[profiles.p]\nagents = ['a','b']\nprojects = ['one','two']")).unwrap();
        assert_eq!(config.select_execution(Some("p"), None).unwrap().0, "b");
        config.profiles.get_mut("p").unwrap().default_agent = Some("a".into());
        assert_eq!(config.select_execution(Some("p"), None).unwrap().0, "a");
        assert_eq!(
            config
                .select_execution(Some("p"), Some("b".into()))
                .unwrap()
                .0,
            "b"
        );
        assert!(
            config
                .select_execution(Some("p"), Some("outside".into()))
                .is_err()
        );
        config.default_agent = None;
        config.profiles.get_mut("p").unwrap().default_agent = None;
        assert!(config.select_execution(Some("p"), None).is_err());
        let profile = &config.profiles["p"];
        assert!(profile.automatic_workspace(None, true).is_err());
        assert!(!profile.automatic_workspace(None, false).unwrap());
        let one = ProjectId::try_from("one".to_owned()).unwrap();
        assert!(
            profile
                .automatic_workspace(Some(one.clone()), true)
                .unwrap()
        );
        assert_eq!(profile.project(None, Some(one.clone())).unwrap(), Some(one));
        assert_eq!(
            profile
                .project(
                    None,
                    Some(ProjectId::try_from("outside".to_owned()).unwrap())
                )
                .unwrap(),
            None
        );
        assert!(
            profile
                .project(
                    Some(ProjectId::try_from("outside".to_owned()).unwrap()),
                    None
                )
                .is_err()
        );
        config.profiles.get_mut("p").unwrap().agents.remove("b");
        config.default_agent = Some("b".into());
        assert_eq!(config.select_execution(Some("p"), None).unwrap().0, "a");
        assert_eq!(config.select_execution(None, None).unwrap().0, "b");
    }

    #[test]
    fn public_profile_summaries_do_not_expose_inline_directory_settings() {
        let config: Config = toml::from_str(&format!("{ENTITIES}\n[profiles.p]\nagents = ['a']\nprojects = ['one']\n[profiles.p.workspaces.one]\npolicy = 'existing'\ndirectory = '/private/directory'")).unwrap();
        let public = serde_json::to_string(&config.public_execution_profiles()).unwrap();
        assert!(public.contains("existing"));
        assert!(!public.contains("private"));
    }
}
