//! Agent lifecycle adapters share durable domain operations, never administrative authority.

use super::*;
use hive_protocol::workspace_tools::{self as tools, ToolError, lifecycle::Request};
use serde_json::{Value, json};

fn failure(error: Error) -> ToolError {
    if let Error::Domain(hive_core::workspace::WorkspaceError::UnsafeCleanup { reason, .. }) = error
    {
        return ToolError::Cleanup(reason);
    }
    ToolError::Lifecycle { code: error.code() }
}

fn scoped_entry<'a>(
    state: &'a State,
    context: &hive_protocol::workspace::Context,
    id: &WorkspaceId,
) -> Result<&'a Entry, ToolError> {
    if context
        .owner
        .as_ref()
        .is_some_and(|owner| owner != &state.node)
    {
        return Err(ToolError::Owner);
    }
    let entry = state.workspaces.get(id.as_str()).ok_or(ToolError::Scope)?;
    if entry.workspace.intent().project.as_str() != context.project
        || entry.workspace.intent().check_node(&state.node).is_err()
    {
        return Err(ToolError::Scope);
    }
    Ok(entry)
}

fn summary(state: &State, entry: &Entry) -> tools::lifecycle::WorkspaceInfo {
    tools::lifecycle::WorkspaceInfo {
        schema_version: tools::VERSION,
        owner: state.node.clone(),
        project: entry.workspace.intent().project.clone(),
        workspace: entry.workspace.id().clone(),
        state: entry.workspace.state(),
        ownership: entry.workspace.intent().ownership(),
        operation: entry.workspace.operation().cloned(),
        failure: entry.failure.clone(),
        retry: entry.workspace.retry(),
        repository_backend: entry
            .selection
            .as_ref()
            .and_then(|s| s.repository.as_ref())
            .map(|r| r.backend.0.clone()),
        provisioner: entry.selection.as_ref().map(|s| s.provisioner.0.clone()),
    }
}

fn metadata(state: &State, entry: &Entry) -> Value {
    // This projection contains only scalar domain values, never backend-controlled payloads.
    json!(summary(state, entry))
}

impl Service {
    pub(super) async fn tool_operation_snapshot(
        &self,
        session: String,
        context: hive_protocol::workspace::Context,
        operation: OperationId,
        authorize: impl Fn() -> Result<(), ToolError> + Send + 'static,
    ) -> Result<Value, ToolError> {
        let store = self.store.clone();
        // A committed SQLite snapshot remains readable while a subprocess owns resources.lock.
        tokio::task::spawn_blocking(move || {
            authorize()?;
            let state = store
                .resource::<State>("node")
                .map_err(|_| ToolError::Backend)?
                .ok_or(ToolError::Scope)?;
            if state.version != 2 {
                return Err(ToolError::Backend);
            }
            let attached = state.uses.get(&session).ok_or(ToolError::Scope)?;
            if attached.as_str() != context.workspace {
                return Err(ToolError::Scope);
            }
            scoped_entry(&state, &context, attached)?;
            let original =
                state
                    .operations
                    .get(operation.as_str())
                    .ok_or(ToolError::Lifecycle {
                        code: "hive::workspace::unknown",
                    })?;
            let entry = scoped_entry(&state, &context, &original.workspace)?;
            let mut result = metadata(&state, entry);
            result["operation"] = json!(operation);
            result["kind"] = json!(original.kind);
            result["effectsStarted"] = json!(original.effects_started);
            result["persistedSnapshot"] = json!(true);
            // Snapshot evidence describes persisted state, not filesystem safety or worker liveness.
            authorize()?;
            Ok(result)
        })
        .await
        .map_err(|_| ToolError::Backend)?
    }
}

impl Runtime<'_> {
    pub(super) fn workspace_tool(
        &mut self,
        session: &str,
        request: Request,
        authorize: &dyn Fn() -> Result<(), ToolError>,
    ) -> Result<Value, ToolError> {
        let current = self
            .state
            .uses
            .get(session)
            .cloned()
            .ok_or(ToolError::Scope)?;
        let parent = self
            .state
            .workspaces
            .get(current.as_str())
            .cloned()
            .ok_or(ToolError::Scope)?;
        let context = hive_protocol::workspace::Context {
            owner: Some(self.state.node.clone()),
            project: parent.workspace.intent().project.to_string(),
            workspace: current.to_string(),
            effective_cwd: String::new(),
        };
        // Earlier phases may already have fetched or preserved data; revoked authority cannot
        // certify no effects and must retain inspect-before-retry semantics for the same ID.
        let fence = || authorize().map_err(|_| Error::Uncertain);
        match request {
            Request::Current | Request::Info { workspace: None } => {
                self.workspace_info(&context, &current)
            }
            Request::Info {
                workspace: Some(id),
            } => self.workspace_info(&context, &id),
            Request::List { limit, cursor } => {
                if !(1..=100).contains(&limit) {
                    return Err(ToolError::Arguments);
                }
                if let Some(cursor) = &cursor {
                    scoped_entry(&self.state, &context, cursor)?;
                }
                let mut entries = self
                    .state
                    .workspaces
                    .values()
                    .filter(|entry| {
                        entry.workspace.intent().project.as_str() == context.project
                            && entry
                                .workspace
                                .intent()
                                .check_node(&self.state.node)
                                .is_ok()
                    })
                    .filter(|entry| {
                        cursor
                            .as_ref()
                            .is_none_or(|cursor| entry.workspace.id() > cursor)
                    });
                let page: Vec<_> = entries.by_ref().take(limit).collect();
                let next = entries
                    .next()
                    .and_then(|_| page.last().map(|entry| entry.workspace.id().clone()));
                Ok(json!(tools::lifecycle::WorkspaceList {
                    schema_version: tools::VERSION,
                    owner: self.state.node.clone(),
                    project: parent.workspace.intent().project.clone(),
                    workspaces: page
                        .iter()
                        .map(|entry| summary(&self.state, entry))
                        .collect(),
                    next_cursor: next,
                }))
            }
            request @ Request::Create { .. } => {
                if let Some(original) = request
                    .operation()
                    .and_then(|id| self.state.operations.get(id.as_str()))
                {
                    // An operation selector must be authorized before comparing its frozen request.
                    scoped_entry(&self.state, &context, &original.workspace)?;
                }
                if !backends::supports_siblings(&parent) {
                    return Err(ToolError::Unsupported);
                }
                let entry = self
                    .create_sibling(&parent, request, &fence)
                    .map_err(failure)?;
                Ok(metadata(&self.state, &entry))
            }
            Request::Remove {
                workspace,
                operation,
            } => {
                scoped_entry(&self.state, &context, &workspace)?;
                let durable = self
                    .state
                    .operations
                    .get(operation.as_str())
                    .map(|original| {
                        if original.workspace != workspace
                            || !matches!(original.kind, Some(OperationKind::Cleanup))
                        {
                            return Err(Error::RetryConflict);
                        }
                        let cleanup: CleanupRequest =
                            serde_json::from_value(original.request.clone())
                                .map_err(|_| Error::RetryConflict)?;
                        if cleanup.mode != CleanupMode::PreserveChanges
                            || cleanup.operation != operation
                            || cleanup.target.workspace != workspace
                            || cleanup.target.check_node(&self.state.node).is_err()
                        {
                            return Err(Error::RetryConflict);
                        }
                        Ok(cleanup)
                    })
                    .transpose()
                    .map_err(failure)?;
                let entry = self
                    .remove_authorized(workspace, operation.clone(), false, durable, &fence)
                    .map_err(failure)?;
                let mut result = metadata(&self.state, &entry);
                result["operation"] = json!(operation);
                Ok(result)
            }
            Request::Operation { .. } => Err(ToolError::Unsupported),
        }
    }

    fn workspace_info(
        &self,
        context: &hive_protocol::workspace::Context,
        id: &WorkspaceId,
    ) -> Result<Value, ToolError> {
        let entry = scoped_entry(&self.state, context, id)?;
        let mut result = metadata(&self.state, entry);
        let inspection = self.inspect(id).map_err(failure)?;
        result["inspection"] = match inspection {
            Inspection::Absent => json!({"presence":"absent"}),
            Inspection::IdentityConflict => json!({"presence":"identity_conflict"}),
            Inspection::Present { safety, .. } => json!({"presence":"present","safety":safety}),
            Inspection::Partial { safety, .. } => json!({"presence":"partial","safety":safety}),
        };
        Ok(result)
    }

    fn create_sibling(
        &mut self,
        parent: &Entry,
        request: Request,
        authorize: &dyn Fn() -> Result<(), Error>,
    ) -> Result<Entry, Error> {
        let Request::Create {
            operation,
            ref base_ref,
            ref branch,
            ref revision,
        } = request.clone()
        else {
            return Err(Error::Request);
        };
        // Freeze submitted intent separately from resolved policy; reloads cannot rewrite retries.
        let frozen = json!({"schema":"hive.workspace.sibling","version":1,"sourceWorkspace":parent.workspace.id(),"request":request});
        if let Some(original) = self.state.operations.get(operation.as_str()).cloned() {
            if original.request != frozen || !original.is_create() {
                return Err(Error::RetryConflict);
            }
            let target = self
                .state
                .workspaces
                .get(original.workspace.as_str())
                .ok_or(Error::Conflict)?;
            if target.workspace.intent().project != parent.workspace.intent().project {
                return Err(Error::RetryConflict);
            }
            target.workspace.intent().check_node(&self.state.node)?;
            return self.resume_creation(&operation, original, authorize);
        }
        if self.operation_reserved(&operation) {
            return Err(Error::RetryConflict);
        }
        if parent.workspace.intent().ownership() != Ownership::HiveManaged {
            return Err(Error::Request);
        }
        let selection = self.selection(parent)?;
        self.manager.resolve(&selection)?;
        let intent =
            backends::sibling_intent(parent, base_ref.clone(), branch.clone(), revision.clone())?;
        self.manager
            .provisioner(&selection)?
            .validate_intent(&intent)?;
        let policy = if matches!(
            parent.workspace.intent().policy,
            WorkspacePolicy::Ephemeral { .. }
        ) {
            WorkspacePolicy::Ephemeral {
                backing: ManagedBacking::Backend {
                    selection: Box::new(selection.clone()),
                    intent,
                },
            }
        } else {
            WorkspacePolicy::Managed {
                selection: Box::new(selection.clone()),
                intent,
            }
        };
        let id = WorkspaceId::new();
        let mut workspace = Workspace::new(
            id.clone(),
            WorkspaceIntent {
                project: parent.workspace.intent().project.clone(),
                node: self.state.node.clone(),
                policy,
                retention: parent.workspace.intent().retention.clone(),
            },
        )?;
        workspace.transition(WorkspaceEvent::Provision {
            operation: operation.clone(),
            backend: parent.workspace.backend().cloned().ok_or(Error::Conflict)?,
        })?;
        let entry = Entry {
            workspace,
            repository: parent.repository.clone(),
            failure: None,
            archive: None,
            selection: Some(selection.clone()),
        };
        self.state.operations.insert(
            operation.to_string(),
            Operation {
                request: frozen,
                workspace: id.clone(),
                commit: None,
                path: None,
                effects_started: false,
                selection: Some(selection),
                kind: Some(OperationKind::Create),
                prepared: None,
            },
        );
        self.state.workspaces.insert(id.to_string(), entry.clone());
        self.save()?;
        self.complete_creation(entry, operation.as_str(), authorize)
    }
}
