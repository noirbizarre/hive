//! Resource administration shares the project's same-origin mutation boundary.
use super::{CreateRequest, Error};
use axum::{
    Json, Router,
    extract::{Path, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use hive_core::{
    identity::{OperationId, ProjectId, RepositoryId, WorkspaceId},
    workspace::GitRef,
};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Clone)]
struct Context {
    broker: crate::Broker,
    origin: Option<String>,
}
type Response = Result<Json<Value>, (StatusCode, Json<Value>)>;

/// v1 exposes Git policy conveniences while storage keeps versioned backend intent.
/// Frozen operation/sweep requests must never be rewritten by a response presenter.
fn present(mut value: Value) -> Value {
    match &mut value {
        Value::Array(items) => {
            for item in items {
                *item = present(item.take());
            }
        }
        Value::Object(fields) => {
            if let Some(workspace) = fields.get_mut("workspace")
                && let Some(policy) = workspace.pointer_mut("/intent/policy")
            {
                let backing = if policy["policy"] == "managed" {
                    Some(policy.clone())
                } else if policy["policy"] == "ephemeral" && policy["backing"]["kind"] == "backend"
                {
                    Some(policy["backing"].clone())
                } else {
                    None
                };
                if let Some(backing) = backing {
                    let id = backing["selection"]["provisioner"]
                        .as_str()
                        .unwrap_or_default();
                    if backing["intent"]["schema"] == "hive.git.intent"
                        && backing["intent"]["version"] == 1
                        && matches!(id, "git.clone" | "git.worktree" | "worktrunk")
                    {
                        let mut legacy = json!({"kind": if id == "git.clone" { "clone" } else { "worktree" }, "git":backing["intent"]["data"]});
                        if id != "git.clone" {
                            legacy["repository"] = backing["selection"]["repository"]["id"].clone();
                        }
                        if policy["policy"] == "ephemeral" {
                            policy["backing"] = legacy;
                        } else {
                            legacy["policy"] = legacy["kind"].take();
                            legacy
                                .as_object_mut()
                                .expect("policy object")
                                .remove("kind");
                            *policy = legacy;
                        }
                    }
                }
            }
            for (key, item) in fields {
                // Never reinterpret opaque backend payloads or immutable request snapshots.
                if !matches!(
                    key.as_str(),
                    "request"
                        | "cleanup"
                        | "metadata"
                        | "observations"
                        | "selection"
                        | "intent"
                        | "handle"
                ) {
                    *item = present(item.take());
                }
            }
        }
        _ => {}
    }
    value
}

fn response(error: Error) -> (StatusCode, Json<Value>) {
    let status = match &error {
        Error::Unknown => StatusCode::NOT_FOUND,
        Error::Request => StatusCode::UNPROCESSABLE_ENTITY,
        Error::Domain(
            hive_core::workspace::WorkspaceError::IncompatibleBackends { .. }
            | hive_core::workspace::WorkspaceError::UnsupportedBackendData { .. },
        ) => StatusCode::UNPROCESSABLE_ENTITY,
        Error::Storage => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::CONFLICT,
    };
    (status, Json(error.json()))
}

impl Context {
    fn authorize(&self, headers: &HeaderMap) -> Result<(), (StatusCode, Json<Value>)> {
        if crate::reload::allowed_origin(headers, self.origin.as_deref()) {
            Ok(())
        } else {
            Err((
                StatusCode::FORBIDDEN,
                Json(
                    json!({"code":"hive::workspace::origin","message":"resource mutation origin is not allowed","help":"use the local CLI or listener origin"}),
                ),
            ))
        }
    }
}

pub(crate) fn router(broker: crate::Broker, origin: Option<String>) -> Router {
    Router::new()
        .route("/v1/repositories", get(repositories))
        .route("/v1/repositories/{id}", get(repository))
        .route("/v1/repositories/{id}/fetch", post(fetch))
        .route("/v1/workspaces", get(list).post(create))
        .route("/v1/workspaces/recovery", get(recovery))
        .route("/v1/workspace-sweeps", post(plan_sweep))
        .route("/v1/workspace-sweeps/{id}", get(sweep))
        .route("/v1/workspace-sweeps/{id}/apply", post(apply_sweep))
        .route("/v1/workspaces/{id}", get(show))
        .route("/v1/workspaces/{id}/inspect", get(inspect))
        .route("/v1/workspaces/{id}/remove", post(remove))
        .route("/v1/workspaces/{id}/reconcile", post(reconcile))
        .route("/v1/operations/{id}", get(operation))
        .with_state(Context { broker, origin })
}

async fn recovery(State(cx): State<Context>) -> Response {
    let projects = cx.broker.recovery_projects().map_err(response)?;
    Ok(Json(present(json!(
        cx.broker
            .workspace_service()
            .map_err(response)?
            .recovery(projects)
            .await
            .map_err(response)?
    ))))
}

async fn plan_sweep(
    State(cx): State<Context>,
    headers: HeaderMap,
    body: Result<Json<super::SweepRequest>, JsonRejection>,
) -> Response {
    cx.authorize(&headers)?;
    let Json(request) = body.map_err(|_| response(Error::Request))?;
    Ok(Json(json!(
        cx.broker
            .workspace_service()
            .map_err(response)?
            .plan_sweep(request)
            .await
            .map_err(response)?
    )))
}

async fn sweep(State(cx): State<Context>, Path(id): Path<String>) -> Response {
    let id = OperationId::try_from(id).map_err(|_| response(Error::Request))?;
    Ok(Json(json!(
        cx.broker
            .workspace_service()
            .map_err(response)?
            .sweep(id)
            .await
            .map_err(response)?
    )))
}

async fn apply_sweep(
    State(cx): State<Context>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    cx.authorize(&headers)?;
    // Applying cannot silently accept replacement intent or destructive flags.
    if !body.is_empty() && serde_json::from_slice::<Value>(&body).ok() != Some(json!({})) {
        return Err(response(Error::Request));
    }
    let id = OperationId::try_from(id).map_err(|_| response(Error::Request))?;
    Ok(Json(json!(
        cx.broker
            .workspace_service()
            .map_err(response)?
            .apply_sweep(id)
            .await
            .map_err(response)?
    )))
}

async fn operation(State(cx): State<Context>, Path(id): Path<String>) -> Response {
    let id = OperationId::try_from(id).map_err(|_| response(Error::Request))?;
    Ok(Json(
        cx.broker
            .workspace_service()
            .map_err(response)?
            .operation(id)
            .await
            .map_err(response)?,
    ))
}

async fn repositories(State(cx): State<Context>) -> Response {
    let service = cx.broker.workspace_service().map_err(response)?;
    Ok(Json(
        json!({"node":service.node().await.map_err(response)?, "repositories":service.repositories().await.map_err(response)?}),
    ))
}
async fn repository(State(cx): State<Context>, Path(id): Path<String>) -> Response {
    let id = RepositoryId::try_from(id).map_err(|_| response(Error::Request))?;
    let repo = cx
        .broker
        .workspace_service()
        .map_err(response)?
        .repositories()
        .await
        .map_err(response)?
        .into_iter()
        .find(|r| r.repository.id == id)
        .ok_or_else(|| response(Error::Unknown))?;
    Ok(Json(json!(repo)))
}
async fn fetch(State(cx): State<Context>, Path(id): Path<String>, headers: HeaderMap) -> Response {
    cx.authorize(&headers)?;
    let id = RepositoryId::try_from(id).map_err(|_| response(Error::Request))?;
    Ok(Json(json!(
        cx.broker
            .workspace_service()
            .map_err(response)?
            .fetch(id)
            .await
            .map_err(response)?
    )))
}
async fn list(State(cx): State<Context>) -> Response {
    Ok(Json(present(
        json!({"workspaces":cx.broker.workspace_service().map_err(response)?.list().await.map_err(response)?}),
    )))
}
async fn show(State(cx): State<Context>, Path(id): Path<String>) -> Response {
    let id = WorkspaceId::try_from(id).map_err(|_| response(Error::Request))?;
    let entry = cx
        .broker
        .workspace_service()
        .map_err(response)?
        .list()
        .await
        .map_err(response)?
        .into_iter()
        .find(|e| e.workspace.id() == &id)
        .ok_or_else(|| response(Error::Unknown))?;
    Ok(Json(present(json!(entry))))
}
async fn inspect(State(cx): State<Context>, Path(id): Path<String>) -> Response {
    let id = WorkspaceId::try_from(id).map_err(|_| response(Error::Request))?;
    Ok(Json(json!(
        cx.broker
            .workspace_service()
            .map_err(response)?
            .inspect(id)
            .await
            .map_err(response)?
    )))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct New {
    revision: Option<hive_core::project::Revision>,
    operation: OperationId,
    project: ProjectId,
    policy: String,
    base_ref: Option<GitRef>,
    branch: Option<GitRef>,
}
async fn create(
    State(cx): State<Context>,
    headers: HeaderMap,
    body: Result<Json<New>, JsonRejection>,
) -> Response {
    cx.authorize(&headers)?;
    let Json(body) = body.map_err(|_| response(Error::Request))?;
    let (policy, definition, worktrunk) = {
        let registry = cx
            .broker
            .registry
            .lock()
            .map_err(|_| response(Error::Storage))?;
        if registry.stopping {
            return Err(response(Error::Storage));
        }
        (
            registry
                .current
                .workspaces
                .get(&body.policy)
                .cloned()
                .ok_or_else(|| response(Error::Request))?,
            registry
                .current
                .projects
                .get(&body.project)
                .or_else(|| registry.managed_projects.get(&body.project))
                .cloned()
                .ok_or_else(|| response(Error::Unknown))?,
            registry.current.worktrunk.unwrap_or_default(),
        )
    };
    let request = CreateRequest {
        operation: body.operation.clone(),
        project: body.project,
        policy,
        base_ref: body.base_ref,
        revision: body.revision,
        branch: body.branch,
    };
    let entry = cx
        .broker
        .workspace_service()
        .map_err(response)?
        .create(request, definition, worktrunk, None)
        .await
        .map_err(|error| {
            let (status, Json(mut value)) = response(error);
            value["operation"] = json!(body.operation);
            (status, Json(value))
        })?;
    Ok(Json(present(json!(entry))))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Remove {
    operation: OperationId,
    #[serde(default)]
    discard_changes: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Reconcile {
    directory: Option<std::path::PathBuf>,
    #[serde(default)]
    hooks_reconciled: bool,
}
async fn reconcile(
    State(cx): State<Context>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Result<Json<Reconcile>, JsonRejection>,
) -> Response {
    cx.authorize(&headers)?;
    let Json(body) = body.map_err(|_| response(Error::Request))?;
    let id = WorkspaceId::try_from(id).map_err(|_| response(Error::Request))?;
    Ok(Json(present(json!(
        cx.broker
            .reconcile_workspace(id, body.directory, body.hooks_reconciled)
            .await
            .map_err(response)?
    ))))
}
async fn remove(
    State(cx): State<Context>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Result<Json<Remove>, JsonRejection>,
) -> Response {
    cx.authorize(&headers)?;
    let Json(body) = body.map_err(|_| response(Error::Request))?;
    let id = WorkspaceId::try_from(id).map_err(|_| response(Error::Request))?;
    Ok(Json(present(json!(
        cx.broker
            .remove_workspace(id, body.operation, body.discard_changes)
            .await
            .map_err(response)?
    ))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v1_policy_projection_preserves_opaque_evidence_and_frozen_requests_in_nested_reports() {
        let policy = json!({"policy":"managed", "selection":{"provisioner":"git.worktree", "repository":{"id":"repository-recorded"}},
            "intent":{"schema":"hive.git.intent", "version":1,"data":{"base_ref":"main","branch":null}}});
        let opaque = json!({"workspace":{"intent":{"policy":policy}}});
        let entry = json!({"workspace":{"intent":{"policy":policy}, "location":{"observations":opaque}}, "selection":opaque});
        let frozen = json!({"workspace":entry, "request":entry, "cleanup":entry});
        let report = present(json!({"workspaces":[frozen]}));
        let projected = &report["workspaces"][0]["workspace"];
        assert_eq!(
            projected["workspace"]["intent"]["policy"],
            json!({"policy":"worktree","repository":"repository-recorded","git":{"base_ref":"main","branch":null}})
        );
        assert_eq!(projected["selection"], opaque);
        assert_eq!(projected["workspace"]["location"]["observations"], opaque);
        assert_eq!(report["workspaces"][0]["request"], entry);
        assert_eq!(report["workspaces"][0]["cleanup"], entry);
    }
}
