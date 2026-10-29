//! Project definitions are inert metadata; registry removal never authorizes filesystem cleanup.

use axum::{
    Json, Router,
    extract::{Path, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
    routing::get,
};
use hive_core::{
    identity::{NodeId, ProjectId},
    project::{ProjectDefinition, ProjectError},
    workspace::{GitRef, NodePath},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{Broker, persistence::StoreError, reload};

#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum InitError {
    #[error(transparent)]
    #[diagnostic(
        code(hive::project::storage),
        help("check the Hive database and restore a backup if stored definitions are invalid")
    )]
    Store(#[from] StoreError),
    #[error(transparent)]
    #[diagnostic(transparent)]
    Config(#[from] crate::config::ConfigError),
    #[error(
        "project '{0}' is both configured and managed; remove or rename one definition before starting Hive"
    )]
    #[diagnostic(
        code(hive::project::collision),
        help("use distinct IDs for configured and managed definitions")
    )]
    Collision(ProjectId),
}

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error(transparent)]
    Invalid(#[from] ProjectError),
    #[error("invalid project request; check IDs, metadata fields and node-local path syntax")]
    Request,
    #[error("project or node override is not registered; list projects and check the ID")]
    Unknown,
    #[error("project ID already exists; use update for managed definitions or choose another ID")]
    Conflict,
    #[error("project is configuration-owned; edit its TOML declaration and run hive daemon reload")]
    ConfigOwned,
    #[error("durable registry unavailable; start a persistent Hive daemon and retry")]
    NoStore,
    #[error("Hive is shutting down; restart the daemon and retry")]
    Stopping,
    #[error(
        "cannot access project registry; check database permissions and disk space, then retry"
    )]
    Storage,
    #[error("project mutation origin is not allowed; use the local CLI or listener origin")]
    Origin,
}

impl RegistryError {
    fn response(self) -> (StatusCode, Json<Value>) {
        let (status, code, help) = match &self {
            Self::Invalid(error) => (StatusCode::UNPROCESSABLE_ENTITY, error.code(), error.help()),
            Self::Request => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "hive::project::request",
                "check the documented request schema; paths require explicit node identity",
            ),
            Self::Unknown => (
                StatusCode::NOT_FOUND,
                "hive::project::unknown",
                "list projects and check the project and node IDs",
            ),
            Self::Conflict => (
                StatusCode::CONFLICT,
                "hive::project::collision",
                "choose a fresh ID or update the existing managed definition",
            ),
            Self::ConfigOwned => (
                StatusCode::CONFLICT,
                "hive::project::config_owned",
                "edit TOML and run hive daemon reload",
            ),
            Self::NoStore => (
                StatusCode::SERVICE_UNAVAILABLE,
                "hive::project::no_store",
                "start a persistent Hive daemon",
            ),
            Self::Stopping => (
                StatusCode::SERVICE_UNAVAILABLE,
                "hive::project::stopping",
                "restart the daemon and retry",
            ),
            Self::Storage => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "hive::project::storage",
                "check database permissions and disk space; inspect daemon logs",
            ),
            Self::Origin => (
                StatusCode::FORBIDDEN,
                "hive::project::origin",
                "use the local CLI or listener origin",
            ),
        };
        (
            status,
            Json(json!({"code":code,"message":self.to_string(),"help":help})),
        )
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct RegisteredProject {
    pub id: ProjectId,
    pub origin: &'static str,
    #[serde(flatten)]
    pub definition: ProjectDefinition,
}

pub struct Registration {
    pub id: Option<ProjectId>,
    pub definition: ProjectDefinition,
}

// Structural decoding stays separate so HTTP can preserve safe domain diagnostics.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DefinitionRequest {
    name: String,
    repository_source: Option<String>,
    default_ref: Option<GitRef>,
    #[serde(default)]
    preparation: Vec<Vec<String>>,
}

impl DefinitionRequest {
    fn checked(self) -> Result<ProjectDefinition, RegistryError> {
        let definition = ProjectDefinition {
            name: self.name,
            repository_source: self.repository_source,
            default_ref: self.default_ref,
            preparation: self.preparation,
        };
        definition.validate()?;
        Ok(definition)
    }
}

struct RegistrationRequest {
    id: Option<ProjectId>,
    definition: DefinitionRequest,
}

impl<'de> Deserialize<'de> for RegistrationRequest {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let mut fields = serde_json::Map::<String, Value>::deserialize(deserializer)?;
        let id = fields
            .remove("id")
            .map(serde_json::from_value)
            .transpose()
            .map_err(D::Error::custom)?
            .flatten();
        // Deserialize the remaining object directly: flatten can bypass nested unknown-field checks.
        let definition = serde_json::from_value(Value::Object(fields)).map_err(D::Error::custom)?;
        Ok(Self { id, definition })
    }
}

struct ProjectSnapshot {
    current: std::sync::Arc<crate::config::Config>,
    managed_projects: std::collections::BTreeMap<ProjectId, ProjectDefinition>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OverrideRequest {
    directory: NodePath,
}

impl Broker {
    pub fn projects(&self) -> Result<Vec<RegisteredProject>, RegistryError> {
        let registry = self.registry.lock().map_err(|_| RegistryError::Storage)?;
        let mut projects: Vec<_> = registry
            .current
            .projects
            .iter()
            .map(|(id, definition)| RegisteredProject {
                id: id.clone(),
                definition: definition.clone(),
                origin: "config",
            })
            .chain(
                registry
                    .managed_projects
                    .iter()
                    .map(|(id, definition)| RegisteredProject {
                        id: id.clone(),
                        definition: definition.clone(),
                        origin: "managed",
                    }),
            )
            .collect();
        projects.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(projects)
    }

    /// The blocking job owns the writer gate through commit and publication even if its caller leaves.
    async fn project_write<T: Send + 'static>(
        &self,
        write: impl FnOnce(&Broker, &mut ProjectSnapshot, &crate::Store) -> Result<T, RegistryError>
        + Send
        + 'static,
    ) -> Result<T, RegistryError> {
        let gate = self.reloading.clone().lock_owned().await;
        let broker = self.clone();
        tokio::task::spawn_blocking(move || {
            let _gate = gate;
            let mut snapshot = {
                let registry = broker.registry.lock().map_err(|_| RegistryError::Storage)?;
                if registry.stopping {
                    return Err(RegistryError::Stopping);
                }
                ProjectSnapshot {
                    current: registry.current.clone(),
                    managed_projects: registry.managed_projects.clone(),
                }
            };
            let store = broker.store.as_ref().ok_or(RegistryError::NoStore)?;
            // The owned writer gate excludes reload and shutdown, without blocking registry readers on SQLite.
            let result = write(&broker, &mut snapshot, store)?;
            let mut registry = broker.registry.lock().map_err(|_| RegistryError::Storage)?;
            registry.managed_projects = snapshot.managed_projects;
            broker.catalog.send_modify(|revision| *revision += 1);
            Ok(result)
        })
        .await
        .map_err(|_| RegistryError::Storage)?
    }

    pub async fn register_project(
        &self,
        request: Registration,
    ) -> Result<RegisteredProject, RegistryError> {
        request.definition.validate()?;
        self.project_write(move |_, registry, store| {
            // Allocate under the writer gate so concurrent registrations cannot choose the same suffix.
            let taken = |id: &ProjectId| {
                registry.current.projects.contains_key(id)
                    || registry.managed_projects.contains_key(id)
            };
            let id = if let Some(id) = request.id {
                id
            } else {
                let base = ProjectId::from_name(&request.definition.name);
                let mut candidate = base.clone();
                let mut suffix = 2_u64;
                while taken(&candidate) {
                    candidate = ProjectId::try_from(format!("{base}-{suffix}"))
                        .expect("a numeric suffix preserves project slug syntax");
                    suffix += 1;
                }
                candidate
            };
            if taken(&id) {
                return Err(RegistryError::Conflict);
            }
            store
                .write_project(&id, Some(&request.definition))
                .map_err(|_| RegistryError::Storage)?;
            registry
                .managed_projects
                .insert(id.clone(), request.definition.clone());
            Ok(RegisteredProject {
                id,
                definition: request.definition,
                origin: "managed",
            })
        })
        .await
    }

    pub async fn replace_project(
        &self,
        id: ProjectId,
        definition: ProjectDefinition,
    ) -> Result<RegisteredProject, RegistryError> {
        definition.validate()?;
        self.project_write(move |_, registry, store| {
            managed(registry, &id)?;
            store
                .write_project(&id, Some(&definition))
                .map_err(|_| RegistryError::Storage)?;
            registry
                .managed_projects
                .insert(id.clone(), definition.clone());
            Ok(RegisteredProject {
                id,
                definition,
                origin: "managed",
            })
        })
        .await
    }

    pub async fn remove_project(&self, id: ProjectId) -> Result<(), RegistryError> {
        self.project_write(move |_, registry, store| {
            managed(registry, &id)?;
            store
                .write_project(&id, None)
                .map_err(|_| RegistryError::Storage)?;
            registry.managed_projects.remove(&id);
            Ok(())
        })
        .await
    }

    pub async fn set_project_override(
        &self,
        id: ProjectId,
        node: NodeId,
        directory: Option<NodePath>,
    ) -> Result<(), RegistryError> {
        if directory
            .as_ref()
            .is_some_and(|directory| directory.node() != &node)
        {
            return Err(RegistryError::Request);
        }
        self.project_write(move |_, registry, store| {
            // Orphaned overrides remain explicitly removable after their definition is detached.
            if directory.is_some()
                && !registry.current.projects.contains_key(&id)
                && !registry.managed_projects.contains_key(&id)
            {
                return Err(RegistryError::Unknown);
            }
            store
                .write_project_override(&id, &node, directory.as_ref())
                .map_err(|_| RegistryError::Storage)
        })
        .await
    }
}

fn managed(registry: &ProjectSnapshot, id: &ProjectId) -> Result<(), RegistryError> {
    if registry.current.projects.contains_key(id) {
        return Err(RegistryError::ConfigOwned);
    }
    if !registry.managed_projects.contains_key(id) {
        return Err(RegistryError::Unknown);
    }
    Ok(())
}

#[derive(Clone)]
struct Context {
    broker: Broker,
    origin: Option<String>,
}

impl Context {
    fn authorize(&self, headers: &HeaderMap) -> Result<(), RegistryError> {
        if reload::allowed_origin(headers, self.origin.as_deref()) {
            Ok(())
        } else {
            Err(RegistryError::Origin)
        }
    }
}

type HttpResult = Result<Json<Value>, (StatusCode, Json<Value>)>;

fn body<T>(body: Result<Json<T>, JsonRejection>) -> Result<T, RegistryError> {
    // Axum's rejection text may contain submitted values; never forward it to clients or logs.
    body.map(|Json(value)| value)
        .map_err(|_| RegistryError::Request)
}

pub(crate) fn router(broker: Broker, origin: Option<String>) -> Router {
    Router::new()
        .route("/v1/projects", get(list).post(register))
        .route("/v1/projects/{id}", get(show).put(replace).delete(remove))
        .route(
            "/v1/projects/{id}/nodes/{node}/override",
            get(show_override).put(set_override).delete(remove_override),
        )
        .with_state(Context { broker, origin })
}

async fn list(State(context): State<Context>) -> HttpResult {
    context
        .broker
        .projects()
        .map(|projects| Json(json!({"projects": projects})))
        .map_err(RegistryError::response)
}

async fn show(State(context): State<Context>, Path(id): Path<String>) -> HttpResult {
    let result = (|| {
        let id = ProjectId::try_from(id).map_err(|_| RegistryError::Request)?;
        let project = context
            .broker
            .projects()?
            .into_iter()
            .find(|project| project.id == id)
            .ok_or(RegistryError::Unknown)?;
        Ok(Json(json!(project)))
    })();
    result.map_err(RegistryError::response)
}

async fn register(
    State(context): State<Context>,
    headers: HeaderMap,
    request: Result<Json<RegistrationRequest>, JsonRejection>,
) -> HttpResult {
    async {
        context.authorize(&headers)?;
        let request = body(request)?;
        let registration = Registration {
            id: request.id,
            definition: request.definition.checked()?,
        };
        Ok(Json(json!(
            context.broker.register_project(registration).await?
        )))
    }
    .await
    .map_err(RegistryError::response)
}

async fn replace(
    State(context): State<Context>,
    Path(id): Path<String>,
    headers: HeaderMap,
    request: Result<Json<DefinitionRequest>, JsonRejection>,
) -> HttpResult {
    async {
        context.authorize(&headers)?;
        let id = ProjectId::try_from(id).map_err(|_| RegistryError::Request)?;
        Ok(Json(json!(
            context
                .broker
                .replace_project(id, body(request)?.checked()?)
                .await?
        )))
    }
    .await
    .map_err(RegistryError::response)
}

async fn remove(
    State(context): State<Context>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> HttpResult {
    async {
        context.authorize(&headers)?;
        context
            .broker
            .remove_project(ProjectId::try_from(id).map_err(|_| RegistryError::Request)?)
            .await?;
        Ok(Json(json!({"removed":true})))
    }
    .await
    .map_err(RegistryError::response)
}

fn ids(id: String, node: String) -> Result<(ProjectId, NodeId), RegistryError> {
    Ok((
        ProjectId::try_from(id).map_err(|_| RegistryError::Request)?,
        NodeId::try_from(node).map_err(|_| RegistryError::Request)?,
    ))
}

async fn show_override(
    State(context): State<Context>,
    Path((id, node)): Path<(String, String)>,
) -> HttpResult {
    let result = (|| {
        let (id, node) = ids(id, node)?;
        let directory = context
            .broker
            .store
            .as_ref()
            .ok_or(RegistryError::NoStore)?
            .project_override(&id, &node)
            .map_err(|_| RegistryError::Storage)?
            .ok_or(RegistryError::Unknown)?;
        Ok(Json(
            json!({"project":id,"node":node,"directory":directory}),
        ))
    })();
    result.map_err(RegistryError::response)
}

async fn set_override(
    State(context): State<Context>,
    Path((id, node)): Path<(String, String)>,
    headers: HeaderMap,
    request: Result<Json<OverrideRequest>, JsonRejection>,
) -> HttpResult {
    async {
        context.authorize(&headers)?;
        let (id, node) = ids(id, node)?;
        context
            .broker
            .set_project_override(id, node, Some(body(request)?.directory))
            .await?;
        Ok(Json(json!({"updated":true})))
    }
    .await
    .map_err(RegistryError::response)
}

async fn remove_override(
    State(context): State<Context>,
    Path((id, node)): Path<(String, String)>,
    headers: HeaderMap,
) -> HttpResult {
    async {
        context.authorize(&headers)?;
        let (id, node) = ids(id, node)?;
        context.broker.set_project_override(id, node, None).await?;
        Ok(Json(json!({"removed":true})))
    }
    .await
    .map_err(RegistryError::response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, ConfigSource};
    use hive_core::workspace::PathSyntax;

    fn definition(name: &str) -> ProjectDefinition {
        serde_json::from_value(
            json!({"name":name,"repository_source":"git@example.test:team/repo"}),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn automatic_slugs_skip_configured_and_managed_ids_even_during_concurrent_registration() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("state/catalog.sqlite3");
        let mut config = Config::default();
        let configured = ProjectId::from_name("Hive");
        config
            .projects
            .insert(configured.clone(), definition("Configured"));
        let broker = Broker::persistent(config.clone(), path.clone()).unwrap();
        let explicit = ProjectId::try_from("hive-2".to_owned()).unwrap();
        broker
            .register_project(Registration {
                id: Some(explicit),
                definition: definition("Explicit"),
            })
            .await
            .unwrap();
        let (first, second) = tokio::join!(
            broker.register_project(Registration {
                id: None,
                definition: definition("Hive")
            }),
            broker.register_project(Registration {
                id: None,
                definition: definition("Hive")
            }),
        );
        let first = first.unwrap();
        let second = second.unwrap();
        let allocated =
            std::collections::BTreeSet::from([first.id.to_string(), second.id.to_string()]);
        assert_eq!(
            allocated,
            std::collections::BTreeSet::from(["hive-3".to_owned(), "hive-4".to_owned()])
        );
        let renamed = broker
            .replace_project(first.id.clone(), definition("Renamed"))
            .await
            .unwrap();
        assert_eq!(renamed.id, first.id);
        assert!(matches!(
            broker
                .register_project(Registration {
                    id: Some(configured),
                    definition: definition("Hive")
                })
                .await,
            Err(RegistryError::Conflict)
        ));
        drop(broker);
        let broker = Broker::persistent(config, path).unwrap();
        let next = broker
            .register_project(Registration {
                id: None,
                definition: definition("Hive"),
            })
            .await
            .unwrap();
        assert_eq!(next.id.as_str(), "hive-5");
        assert!(
            broker
                .projects()
                .unwrap()
                .iter()
                .any(|project| project.id == first.id && project.definition.name == "Renamed")
        );
    }

    #[tokio::test]
    async fn a_waiting_database_writer_does_not_block_project_inventory_reads() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("state/catalog.sqlite3");
        let broker = Broker::persistent(Config::default(), path.clone()).unwrap();
        let db = rusqlite::Connection::open(path).unwrap();
        db.execute_batch("BEGIN IMMEDIATE;").unwrap();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let writer = tokio::spawn({
            let broker = broker.clone();
            async move {
                broker
                    .project_write(move |_, snapshot, store| {
                        entered_tx.send(()).unwrap();
                        let id = ProjectId::new();
                        let definition = definition("Waiting");
                        store
                            .write_project(&id, Some(&definition))
                            .map_err(|_| RegistryError::Storage)?;
                        snapshot.managed_projects.insert(id, definition);
                        Ok(())
                    })
                    .await
            }
        });
        entered_rx.await.unwrap();
        let reader = tokio::task::spawn_blocking({
            let broker = broker.clone();
            move || broker.projects()
        });
        let inventory = tokio::time::timeout(std::time::Duration::from_secs(1), reader).await;
        // Release the external lock even if the responsiveness assertion fails.
        db.execute_batch("ROLLBACK;").unwrap();
        assert!(inventory.unwrap().unwrap().unwrap().is_empty());
        writer.await.unwrap().unwrap();
        assert_eq!(broker.projects().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn override_routes_preserve_configured_ownership_and_reject_invalid_or_foreign_writes() {
        let temporary = tempfile::tempdir().unwrap();
        let id = ProjectId::new();
        let node = NodeId::new();
        let other = NodeId::new();
        let mut config = Config::default();
        config.projects.insert(id.clone(), definition("Configured"));
        let broker =
            Broker::persistent(config, temporary.path().join("state/catalog.sqlite3")).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(
            axum::serve(listener, crate::router_with_origin(broker, Some(&base))).into_future(),
        );
        let client = reqwest::Client::new();
        let route = format!("{base}/v1/projects/{id}/nodes/{node}/override");
        let directory =
            json!({"directory":{"node":node,"syntax":"posix","path":"/node/local/repo"}});
        assert_eq!(
            client
                .put(&route)
                .json(&directory)
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
        assert_eq!(
            client
                .get(&route)
                .send()
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap()["directory"]["path"],
            "/node/local/repo"
        );
        let mismatch = json!({"directory":{"node":other,"syntax":"posix","path":"/other/repo"}});
        assert_eq!(
            client
                .put(&route)
                .json(&mismatch)
                .send()
                .await
                .unwrap()
                .status(),
            422
        );
        assert_eq!(
            client
                .put(&route)
                .header("origin", "https://foreign.test")
                .json(&directory)
                .send()
                .await
                .unwrap()
                .status(),
            403
        );
        assert_eq!(
            client
                .delete(&route)
                .header("origin", "https://foreign.test")
                .send()
                .await
                .unwrap()
                .status(),
            403
        );
        let missing = format!(
            "{base}/v1/projects/{}/nodes/{node}/override",
            ProjectId::new()
        );
        assert_eq!(
            client
                .put(missing)
                .json(&directory)
                .send()
                .await
                .unwrap()
                .status(),
            404
        );
        assert_eq!(client.delete(&route).send().await.unwrap().status(), 200);
        assert_eq!(client.get(&route).send().await.unwrap().status(), 404);
        server.abort();
    }

    #[tokio::test]
    async fn registry_updates_survive_restart_and_removal_preserves_local_directories_and_overrides()
     {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("state/catalog.sqlite3");
        let sentinel = temporary.path().join("user-checkout");
        std::fs::create_dir(&sentinel).unwrap();
        std::fs::write(sentinel.join("uncommitted"), "keep").unwrap();
        let broker = Broker::persistent(Config::default(), path.clone()).unwrap();
        let registered = broker
            .register_project(Registration {
                id: None,
                definition: definition("First"),
            })
            .await
            .unwrap();
        let id = registered.id;
        assert_eq!(std::fs::read_dir(temporary.path()).unwrap().count(), 2);
        let node = NodeId::new();
        let other = NodeId::new();
        let local = NodePath::new(
            node.clone(),
            if cfg!(windows) {
                PathSyntax::Windows
            } else {
                PathSyntax::Posix
            },
            sentinel.to_str().unwrap().into(),
        )
        .unwrap();
        let remote = NodePath::new(
            other.clone(),
            PathSyntax::Windows,
            "C:\\projects\\repo".into(),
        )
        .unwrap();
        broker
            .set_project_override(id.clone(), node.clone(), Some(local.clone()))
            .await
            .unwrap();
        broker
            .set_project_override(id.clone(), other.clone(), Some(remote.clone()))
            .await
            .unwrap();
        assert!(matches!(
            broker
                .set_project_override(id.clone(), other.clone(), Some(local.clone()))
                .await,
            Err(RegistryError::Request)
        ));
        let renamed = broker
            .replace_project(id.clone(), definition("Renamed"))
            .await
            .unwrap();
        assert_eq!(renamed.id, id);
        assert!(
            !serde_json::to_string(&renamed)
                .unwrap()
                .contains("user-checkout")
        );
        drop(broker);
        let broker = Broker::persistent(Config::default(), path.clone()).unwrap();
        assert_eq!(broker.projects().unwrap()[0].definition.name, "Renamed");
        assert_eq!(
            broker
                .store
                .as_ref()
                .unwrap()
                .project_override(&id, &other)
                .unwrap(),
            Some(remote)
        );
        broker.remove_project(id.clone()).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(sentinel.join("uncommitted")).unwrap(),
            "keep"
        );
        assert_eq!(
            broker
                .store
                .as_ref()
                .unwrap()
                .project_override(&id, &node)
                .unwrap(),
            Some(local)
        );
        drop(broker);
        let broker = Broker::persistent(Config::default(), path).unwrap();
        assert!(broker.projects().unwrap().is_empty());
        broker
            .set_project_override(id.clone(), node.clone(), None)
            .await
            .unwrap();
        assert!(
            broker
                .store
                .as_ref()
                .unwrap()
                .project_override(&id, &node)
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn configured_definitions_are_read_only_and_collisions_preserve_reload_state() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("hive.toml");
        let path = temporary.path().join("state/catalog.sqlite3");
        let configured = ProjectId::new();
        std::fs::write(
            &root,
            format!("[projects.{configured}]\nname = 'Configured'\n"),
        )
        .unwrap();
        let source = ConfigSource::new(Some(root.clone())).unwrap();
        let broker = Broker::persistent(source.load().unwrap(), path.clone())
            .unwrap()
            .with_config_source(source);
        assert!(matches!(
            broker.remove_project(configured.clone()).await,
            Err(RegistryError::ConfigOwned)
        ));
        assert!(matches!(
            broker
                .replace_project(configured.clone(), definition("No"))
                .await,
            Err(RegistryError::ConfigOwned)
        ));
        assert!(matches!(
            broker
                .register_project(Registration {
                    id: Some(configured),
                    definition: definition("No")
                })
                .await,
            Err(RegistryError::Conflict)
        ));
        let registered = broker
            .register_project(Registration {
                id: None,
                definition: definition("Managed"),
            })
            .await
            .unwrap();
        let revision = *broker.catalog.borrow();
        std::fs::write(
            &root,
            format!("[projects.{}]\nname = 'Collision'\n", registered.id),
        )
        .unwrap();
        assert!(matches!(
            broker.reload_config().await,
            Err(crate::reload::ReloadError::ProjectCollision(_))
        ));
        assert_eq!(*broker.catalog.borrow(), revision);
        assert_eq!(broker.projects().unwrap().len(), 2);
        let candidate = ConfigSource::new(Some(root)).unwrap().load().unwrap();
        assert!(matches!(
            Broker::persistent(candidate, path),
            Err(InitError::Collision(_))
        ));
    }

    #[tokio::test]
    async fn a_database_write_failure_preserves_the_published_definition_and_revision() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("state/catalog.sqlite3");
        let broker = Broker::persistent(Config::default(), path.clone()).unwrap();
        let project = broker
            .register_project(Registration {
                id: None,
                definition: definition("Original"),
            })
            .await
            .unwrap();
        let revision = *broker.catalog.borrow();
        let db = rusqlite::Connection::open(path).unwrap();
        db.execute_batch("CREATE TRIGGER reject_project_update BEFORE UPDATE ON projects BEGIN SELECT RAISE(ABORT, 'write rejected'); END;").unwrap();
        assert!(matches!(
            broker
                .replace_project(project.id, definition("Rejected"))
                .await,
            Err(RegistryError::Storage)
        ));
        assert_eq!(*broker.catalog.borrow(), revision);
        assert_eq!(broker.projects().unwrap()[0].definition.name, "Original");
        assert_eq!(
            broker
                .store
                .as_ref()
                .unwrap()
                .projects()
                .unwrap()
                .values()
                .next()
                .unwrap()
                .name,
            "Original"
        );
    }

    #[tokio::test]
    async fn concurrent_reload_and_registration_cannot_publish_the_same_project_id() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("hive.toml");
        std::fs::write(&root, "").unwrap();
        let source = ConfigSource::new(Some(root.clone())).unwrap();
        let broker = Broker::persistent(
            source.load().unwrap(),
            temporary.path().join("state/catalog.sqlite3"),
        )
        .unwrap()
        .with_config_source(source);
        let id = ProjectId::new();
        std::fs::write(root, format!("[projects.{id}]\nname = 'Configured'\n")).unwrap();
        let (reload, create) = tokio::join!(
            broker.reload_config(),
            broker.register_project(Registration {
                id: Some(id),
                definition: definition("Managed")
            })
        );
        assert_ne!(reload.is_ok(), create.is_ok());
        assert_eq!(broker.projects().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn cancelling_a_writer_cannot_leave_a_committed_definition_unpublished() {
        let temporary = tempfile::tempdir().unwrap();
        let broker = Broker::persistent(
            Config::default(),
            temporary.path().join("state/catalog.sqlite3"),
        )
        .unwrap();
        let id = ProjectId::new();
        let saved = id.clone();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let writer = tokio::spawn({
            let broker = broker.clone();
            async move {
                broker
                    .project_write(move |_, registry, store| {
                        entered_tx.send(()).unwrap();
                        release_rx.recv().unwrap();
                        let definition = definition("Committed");
                        store
                            .write_project(&id, Some(&definition))
                            .map_err(|_| RegistryError::Storage)?;
                        registry.managed_projects.insert(id, definition);
                        Ok(())
                    })
                    .await
            }
        });
        entered_rx.await.unwrap();
        writer.abort();
        release_tx.send(()).unwrap();
        // Acquiring the writer gate acknowledges that the detached publication job finished.
        let _gate = broker.reloading.lock().await;
        assert_eq!(broker.projects().unwrap()[0].id, saved);
        assert_eq!(*broker.catalog.borrow(), 1);
        assert!(
            broker
                .store
                .as_ref()
                .unwrap()
                .projects()
                .unwrap()
                .contains_key(&saved)
        );
    }

    #[tokio::test]
    async fn http_crud_validates_metadata_and_rejects_cross_origin_mutations() {
        let temporary = tempfile::tempdir().unwrap();
        let broker = Broker::persistent(
            Config::default(),
            temporary.path().join("state/catalog.sqlite3"),
        )
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(
            axum::serve(
                listener,
                crate::router_with_origin(broker.clone(), Some(&url)),
            )
            .into_future(),
        );
        let client = reqwest::Client::new();
        let endpoint = format!("{url}/v1/projects");
        for value in [
            json!({}),
            json!({"name":"Bad", "repository_source":"https://user:private-token@example.test/repo"}),
            json!({"name":"Bad", "extra":"private-token"}),
        ] {
            let response = client.post(&endpoint).json(&value).send().await.unwrap();
            assert_eq!(response.status(), 422);
            assert!(!response.text().await.unwrap().contains("private-token"));
        }
        let response = client
            .post(&endpoint)
            .header("origin", "https://foreign.test")
            .json(&json!({"name":"No"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 403);
        let response = client
            .post(&endpoint)
            .json(&json!({"name":"Created"}))
            .send()
            .await
            .unwrap();
        assert!(
            response.status().is_success(),
            "{}",
            response.text().await.unwrap()
        );
        let created: Value = response.json().await.unwrap();
        assert_eq!(created["id"], "created");
        let item = format!("{endpoint}/{}", created["id"].as_str().unwrap());
        for (value, code) in [
            (json!({"name":" "}), "hive::project::name"),
            (
                json!({"name":"Bad", "repository_source":"https://user:private-token@example.test/repo"}),
                "hive::project::remote",
            ),
            (
                json!({"name":"Bad", "preparation":[[]]}),
                "hive::project::preparation",
            ),
        ] {
            for request in [client.post(&endpoint), client.put(&item)] {
                let response = request.json(&value).send().await.unwrap();
                assert_eq!(response.status(), 422);
                let error: Value = response.json().await.unwrap();
                assert_eq!(error["code"], code);
                assert!(!error["help"].as_str().unwrap().is_empty());
                assert!(!error.to_string().contains("private-token"));
            }
        }
        assert_eq!(created["origin"], "managed");
        assert_eq!(client.get(&item).send().await.unwrap().status(), 200);
        assert_eq!(
            client
                .put(&item)
                .header("origin", "https://foreign.test")
                .json(&json!({"name":"No"}))
                .send()
                .await
                .unwrap()
                .status(),
            403
        );
        assert_eq!(
            client
                .put(&item)
                .json(&json!({"name":"Updated"}))
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
        assert_eq!(
            client
                .delete(&item)
                .header("sec-fetch-site", "cross-site")
                .send()
                .await
                .unwrap()
                .status(),
            403
        );
        assert_eq!(client.delete(&item).send().await.unwrap().status(), 200);
        assert_eq!(client.get(&item).send().await.unwrap().status(), 404);
        assert!(broker.projects().unwrap().is_empty());
        server.abort();
    }
}
