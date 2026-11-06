//! One native ACP attachment aggregates session-authorized legacy stdio backends.

use super::{
    SessionMcpLease,
    configuration::PreparedServer,
    namespace,
    pagination::{Cursors, Page},
    runtime::{McpRequest, RuntimeError},
};
use agent_client_protocol::{
    Agent, Error,
    mcp_server::{McpOperationCancellation, McpOutcome, McpRequestContext, McpService},
};
use futures::{FutureExt, future::BoxFuture};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

pub(crate) const NAME: &str = "hive-managed-mcp";
type Selection = BTreeMap<String, PreparedServer>;

#[derive(Clone, Default)]
pub(crate) struct Bindings {
    pub selection: Selection,
    leases: BTreeMap<String, SessionMcpLease>,
}

impl Bindings {
    pub(crate) fn new(selection: Selection) -> Self {
        Self {
            selection,
            leases: BTreeMap::new(),
        }
    }
}

pub(crate) fn select(
    config: &crate::reload::PreparedConfig,
    selectors: &[String],
) -> Result<Selection, RuntimeError> {
    if selectors.len() > 32
        || selectors
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            != selectors.len()
    {
        return Err(RuntimeError::Selection);
    }
    selectors
        .iter()
        .map(|id| {
            config
                .servers
                .get(id)
                .cloned()
                .map(|s| (id.clone(), s))
                .ok_or(RuntimeError::Selection)
        })
        .collect()
}

#[cfg(test)]
pub(crate) fn forward_http(
    selected: &Selection,
    declarations: &mut Vec<hive_protocol::v2::McpServer>,
) -> Result<(), RuntimeError> {
    forward_http_with_builtin(selected, declarations, false)
}

pub(crate) fn forward_http_with_builtin(
    selected: &Selection,
    declarations: &mut Vec<hive_protocol::v2::McpServer>,
    builtin: bool,
) -> Result<(), RuntimeError> {
    // Validate collisions before mutating so a rejected setup has no partial declaration set.
    let mut names = std::collections::BTreeSet::new();
    for declaration in declarations.iter() {
        let value = serde_json::to_value(declaration).map_err(|_| RuntimeError::Request)?;
        if let Some(name) = value.get("name").and_then(Value::as_str) {
            names.insert(name.to_owned());
        }
    }
    for (id, server) in selected {
        if !server.managed() && !names.insert(id.clone()) {
            return Err(RuntimeError::Declarations);
        }
    }
    if (builtin || selected.values().any(PreparedServer::managed)) && names.contains(NAME) {
        return Err(RuntimeError::Declarations);
    }
    for (id, server) in selected {
        if let PreparedServer::Http { url, headers } = server {
            declarations.push(serde_json::from_value(json!({"type":"http", "name":id, "url":url,
                "headers": headers.iter().map(|(name,value)| json!({"name":name,"value":value})).collect::<Vec<_>>()
            })).map_err(|_| RuntimeError::Request)?);
        }
    }
    Ok(())
}

#[derive(Clone)]
pub(crate) struct Aggregate(Arc<State>);
struct State {
    workspace: Option<super::workspace::Provider>,
    backends: Vec<(String, SessionMcpLease)>,
    names: Mutex<namespace::Names>,
    cursors: Mutex<Cursors>,
}

impl Aggregate {
    pub async fn prepare_with_workspace(
        broker: &crate::Broker,
        session: &hive_protocol::v2::SessionId,
        selected: Selection,
        workspace_enabled: bool,
    ) -> Result<Option<Self>, String> {
        let workspace = if workspace_enabled {
            super::workspace::Provider::prepare(broker, session).map_err(diagnostic)?
        } else {
            None
        };
        let mut retained = broker
            .mcp_sessions
            .configured
            .lock()
            .map_err(|_| diagnostic(RuntimeError::Binding))?
            .get(&session.to_string())
            .cloned()
            .unwrap_or_else(|| Bindings {
                selection: selected.clone(),
                leases: BTreeMap::new(),
            });
        let mut backends = Vec::new();
        for (id, server) in selected.into_iter().filter(|(_, s)| s.managed()) {
            if let Some(lease) = retained.leases.get(&id).filter(|lease| lease.bound()) {
                backends.push((id, lease.clone()));
                continue;
            }
            let node = broker.mcp_execution_node().await.map_err(diagnostic)?;
            let lease = broker
                .bind_configured_mcp(session, id.clone(), server, node)
                .await
                .map_err(diagnostic)?;
            retained.leases.insert(id.clone(), lease.clone());
            backends.push((id, lease));
        }
        {
            let mut configured = broker
                .mcp_sessions
                .configured
                .lock()
                .map_err(|_| diagnostic(RuntimeError::Binding))?;
            // Teardown removes this entry. A racing preparation must never resurrect it.
            *configured
                .get_mut(&session.to_string())
                .ok_or_else(|| diagnostic(RuntimeError::Binding))? = retained;
        }
        if backends.is_empty() && workspace.is_none() {
            return Ok(None);
        }
        let aggregate = Self(Arc::new(State {
            workspace,
            backends,
            names: Mutex::new(namespace::Names::default()),
            cursors: Mutex::new(Cursors::default()),
        }));
        // Acquire starts lazily; explicit describe waits for real initialization before agent setup.
        futures::future::try_join_all(
            aggregate
                .0
                .backends
                .iter()
                .map(|(_, lease)| call(lease, McpRequest::describe(), None)),
        )
        .await
        .map_err(diagnostic)?;
        Ok(Some(aggregate))
    }

    async fn execute_inner(
        &self,
        method: &str,
        mut params: Value,
        cancellation: &McpOperationCancellation,
    ) -> Result<Value, RuntimeError> {
        if cancellation.is_cancelled() {
            return Err(RuntimeError::Cancelled);
        }
        if ["task", "inputResponses", "requestState"]
            .iter()
            .any(|key| params.get(key).is_some())
        {
            return Err(RuntimeError::Request);
        }
        if let Some(meta) = params.get_mut("_meta").and_then(Value::as_object_mut) {
            // Modern per-request negotiation must not alter the shared backend's legacy initialization.
            meta.retain(|key, _| {
                !key.starts_with("io.modelcontextprotocol/") && key != "progressToken"
            });
        }
        let mut result = match method {
            "server/discover" => {
                let mut capabilities = serde_json::Map::new();
                if let Some(provider) = &self.0.workspace {
                    provider.capabilities(Some(cancellation)).await?;
                    capabilities.insert("tools".into(), json!({}));
                }
                for (_, lease) in &self.0.backends {
                    let metadata = call(lease, McpRequest::describe(), Some(cancellation)).await?;
                    for family in ["tools", "resources", "prompts"] {
                        if metadata["capabilities"]
                            .get(family)
                            .is_some_and(Value::is_object)
                        {
                            capabilities.insert(family.into(), json!({}));
                        }
                    }
                }
                json!({"supportedVersions":["2026-07-28"], "capabilities":capabilities,
                    "_meta":{"io.modelcontextprotocol/serverInfo":{"name":NAME,"version":env!("CARGO_PKG_VERSION")}}})
            }
            "ping" => {
                for (_, lease) in &self.0.backends {
                    call(
                        lease,
                        McpRequest::new("ping", json!({}))?,
                        Some(cancellation),
                    )
                    .await?;
                }
                json!({})
            }
            "tools/list" | "prompts/list" | "resources/list" | "resources/templates/list" => {
                self.list(method, params, cancellation).await?
            }
            "tools/call" | "prompts/get" | "resources/read" => {
                if method == "tools/call"
                    && params
                        .get("name")
                        .and_then(Value::as_str)
                        .is_some_and(|name| name.starts_with("hive."))
                {
                    self.workspace_call(params, cancellation).await?
                } else {
                    let (backend, original) = if method == "resources/read" {
                        namespace::unwrap(
                            self.0.backends.iter().map(|(id, _)| id.as_str()),
                            params
                                .get("uri")
                                .and_then(Value::as_str)
                                .ok_or(RuntimeError::Request)?,
                        )?
                    } else {
                        self.0
                            .names
                            .lock()
                            .map_err(|_| RuntimeError::Binding)?
                            .resolve(
                                if method == "tools/call" { "t" } else { "p" },
                                params
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .ok_or(RuntimeError::Request)?,
                            )?
                    };
                    params[if method == "resources/read" {
                        "uri"
                    } else {
                        "name"
                    }] = json!(original);
                    let lease = &self
                        .0
                        .backends
                        .iter()
                        .find(|(id, _)| id == &backend)
                        .ok_or(RuntimeError::Request)?
                        .1;
                    let mut result =
                        call(lease, McpRequest::new(method, params)?, Some(cancellation)).await?;
                    namespace::rewrite_result(&backend, method, &mut result)?;
                    result
                }
            }
            _ => return Err(RuntimeError::Method),
        };
        let object = result.as_object_mut().ok_or(RuntimeError::Response)?;
        object.insert("resultType".into(), json!("complete"));
        if matches!(
            method,
            "server/discover"
                | "tools/list"
                | "prompts/list"
                | "resources/list"
                | "resources/templates/list"
                | "resources/read"
        ) {
            object.insert("ttlMs".into(), json!(0));
            object.insert("cacheScope".into(), json!("private"));
        }
        if serde_json::to_vec(&result)
            .map_err(|_| RuntimeError::Response)?
            .len()
            > 8 * 1024 * 1024
        {
            return Err(RuntimeError::Response);
        }
        Ok(result)
    }

    async fn list(
        &self,
        method: &str,
        params: Value,
        cancellation: &McpOperationCancellation,
    ) -> Result<Value, RuntimeError> {
        if method == "tools/list"
            && params.get("cursor").is_none()
            && let Some(provider) = &self.0.workspace
        {
            let capabilities = provider.capabilities(Some(cancellation)).await?;
            let descriptors: Vec<_> = capabilities
                .tools
                .iter()
                .map(|tool| hive_protocol::workspace_tools::tool_descriptor(&tool.name))
                .collect();
            let mut result = json!({"tools":descriptors});
            if !self.0.backends.is_empty() {
                result["nextCursor"] = json!(
                    self.0
                        .cursors
                        .lock()
                        .map_err(|_| RuntimeError::Binding)?
                        .insert(Page::first(method))?
                );
            }
            return Ok(result);
        }
        let mut page = match params.get("cursor") {
            None => Page::first(method),
            Some(Value::String(token)) => self
                .0
                .cursors
                .lock()
                .map_err(|_| RuntimeError::Binding)?
                .get(method, token)?,
            _ => return Err(RuntimeError::Request),
        };
        let key = match method {
            "tools/list" => "tools",
            "prompts/list" => "prompts",
            "resources/list" => "resources",
            _ => "resourceTemplates",
        };
        while let Some((backend, lease)) = self.0.backends.get(page.backend) {
            let metadata = call(lease, McpRequest::describe(), Some(cancellation)).await?;
            let generation = metadata["generation"]
                .as_str()
                .ok_or(RuntimeError::Response)?
                .to_owned();
            if page.generation.as_ref().is_some_and(|g| g != &generation) {
                return Err(RuntimeError::Request);
            }
            let family = method.split('/').next().ok_or(RuntimeError::Request)?;
            if !metadata["capabilities"]
                .get(family)
                .is_some_and(Value::is_object)
            {
                page.backend += 1;
                continue;
            }
            let mut backend_params = params.clone();
            backend_params
                .as_object_mut()
                .ok_or(RuntimeError::Request)?
                .remove("cursor");
            if let Some(cursor) = &page.cursor {
                backend_params["cursor"] = json!(cursor);
            }
            let mut result = call(
                lease,
                McpRequest::new(method, backend_params)?.in_generation(generation.clone()),
                Some(cancellation),
            )
            .await?;
            let items = result
                .get_mut(key)
                .and_then(Value::as_array_mut)
                .ok_or(RuntimeError::Response)?;
            if items.len() > 1024 {
                return Err(RuntimeError::Response);
            }
            for item in items {
                match method {
                    "tools/list" | "prompts/list" => {
                        if method == "tools/list"
                            && !item.get("inputSchema").is_some_and(Value::is_object)
                        {
                            return Err(RuntimeError::Response);
                        }
                        let original = item
                            .get("name")
                            .and_then(Value::as_str)
                            .ok_or(RuntimeError::Response)?;
                        // Digest aliases stay bounded; retain a human/agent-readable source identity.
                        let title = format!(
                            "{backend}: {}",
                            item.get("title")
                                .and_then(Value::as_str)
                                .unwrap_or(original)
                        );
                        let alias = self
                            .0
                            .names
                            .lock()
                            .map_err(|_| RuntimeError::Binding)?
                            .expose(
                                if method == "tools/list" { "t" } else { "p" },
                                backend,
                                original,
                            )?;
                        item["name"] = json!(alias);
                        item["title"] = json!(title);
                    }
                    "resources/list" => {
                        if !item.get("name").is_some_and(Value::is_string) {
                            return Err(RuntimeError::Response);
                        }
                        namespace::field(backend, item, "uri")?;
                    }
                    _ => {
                        if !item.get("name").is_some_and(Value::is_string) {
                            return Err(RuntimeError::Response);
                        }
                        namespace::field(backend, item, "uriTemplate")?;
                    }
                }
            }
            let cursor = match result.get("nextCursor") {
                None => None,
                Some(Value::String(c)) if !c.is_empty() => Some(c.clone()),
                _ => return Err(RuntimeError::Response),
            };
            page.advance(cursor, generation)?;
            result
                .as_object_mut()
                .ok_or(RuntimeError::Response)?
                .remove("nextCursor");
            if page.backend < self.0.backends.len() {
                result["nextCursor"] = json!(
                    self.0
                        .cursors
                        .lock()
                        .map_err(|_| RuntimeError::Binding)?
                        .insert(page)?
                );
            }
            return Ok(result);
        }
        Ok(json!({key:[]}))
    }

    async fn workspace_call(
        &self,
        params: Value,
        cancellation: &McpOperationCancellation,
    ) -> Result<Value, RuntimeError> {
        use hive_protocol::workspace_tools::{self as tools, ToolError};
        let provider = self.0.workspace.as_ref().ok_or(ToolError::Unsupported)?;
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or(ToolError::Arguments)?;
        let request = if name == tools::CAPABILITIES {
            tools::validate_arguments(params.get("arguments"))?;
            None
        } else {
            Some(tools::request(name, params.get("arguments"))?)
        };
        let result = match provider.execute(request, Some(cancellation)).await {
            Ok(value) => {
                json!({"structuredContent":value,"content":[{"type":"text","text":value.to_string()}],"isError":false})
            }
            Err(RuntimeError::WorkspaceTool(error)) => {
                json!({"isError":true,"content":[{"type":"text","text":error.data().to_string()}]})
            }
            Err(error) => return Err(error),
        };
        Ok(result)
    }
}

impl McpService<Agent> for Aggregate {
    fn execute(
        &self,
        request: agent_client_protocol::mcp_server::McpRequest,
        context: McpRequestContext<Agent>,
    ) -> BoxFuture<'static, Result<McpOutcome, Error>> {
        let this = self.clone();
        async move {
            match this.execute_inner(&request.method, Value::Object(request.params.unwrap_or_default()), context.operation_cancellation()).await {
                Ok(result) => Ok(McpOutcome::Result(result)),
                Err(RuntimeError::WorkspaceTool(error)) => {
                    use hive_protocol::workspace_tools::ToolError;
                    let code = if matches!(error, ToolError::Arguments | ToolError::Unsupported) { -32602 } else { -32000 };
                    Ok(McpOutcome::Error(serde_json::from_value(json!({"code":code,"message":error.to_string(),"data":error.data()})).expect("fixed workspace tool error")))
                },
                Err(RuntimeError::Method) => Ok(McpOutcome::Error(serde_json::from_value(json!({"code":-32601,"message":"unsupported MCP method; use the facade's supported discovery, tool, resource or prompt operations"})).expect("fixed MCP error"))),
                Err(RuntimeError::Request) => Ok(McpOutcome::Error(serde_json::from_value(json!({"code":-32602,"message":"unsupported MCP operation or invalid Hive identifier/cursor; discover again and check arguments"})).expect("fixed MCP error"))),
                Err(RuntimeError::Remote { code }) => Ok(McpOutcome::Error(serde_json::from_value(json!({"code":code,"message":"MCP backend rejected the request; check arguments and inspect side effects before retrying","data":{"code":"hive::mcp::remote"}})).expect("fixed MCP error"))),
                // The SDK wraps Display in its backend-failure carrier, so put actionable text there.
                Err(error) => Err(Error::new(-32603, diagnostic(error))),
            }
        }.boxed()
    }
}

async fn call(
    lease: &SessionMcpLease,
    request: McpRequest,
    cancellation: Option<&McpOperationCancellation>,
) -> Result<Value, RuntimeError> {
    if cancellation.is_some_and(McpOperationCancellation::is_cancelled) {
        return Err(RuntimeError::Cancelled);
    }
    let call = lease.request(request)?;
    let cancel = call.cancel.clone();
    let mut done = call.done.clone();
    let response = call.result();
    tokio::pin!(response);
    let result = tokio::select! {
        result = &mut response => result,
        _ = async { match cancellation { Some(c) => c.cancelled().await, None => std::future::pending().await } } => {
            cancel.send_replace(true);
            response.await
        }
    };
    // The SDK requires owned cleanup, and caller-visible cancellation can precede settlement.
    while !*done.borrow_and_update() {
        if done.changed().await.is_err() {
            break;
        }
    }
    result
}

pub(crate) fn diagnostic(error: impl std::fmt::Display + miette::Diagnostic) -> String {
    format!(
        "{}: {error}",
        error
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "hive::mcp::routing".into())
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_http_appends_without_changing_inline_declarations_and_collisions_are_atomic() {
        let original: hive_protocol::v2::McpServer = serde_json::from_value(json!({
            "type":"http", "name":"inline", "url":"https://inline.example.test/exact?query=1", "headers":[{"name":"Authorization","value":"private-value"}]
        })).unwrap();
        let configured: crate::mcp::configuration::ConfiguredServer = serde_json::from_value(
            json!({"url":"https://configured.example.test/mcp","headers":{}}),
        )
        .unwrap();
        let configured = configured.prepare(&BTreeMap::new()).unwrap();
        let selected = BTreeMap::from([("configured".into(), configured.clone())]);
        let mut declarations = vec![original.clone()];
        forward_http(&selected, &mut declarations).unwrap();
        assert_eq!(declarations[0], original);
        assert_eq!(declarations.len(), 2);
        let before = declarations.clone();
        assert_eq!(
            forward_http(
                &BTreeMap::from([("inline".into(), configured)]),
                &mut declarations
            ),
            Err(RuntimeError::Declarations)
        );
        assert_eq!(declarations, before);
    }

    #[test]
    fn a_built_in_only_aggregate_reserves_its_declaration_name_atomically() {
        let declaration = serde_json::from_value(
            json!({"type":"http","name":NAME,"url":"https://example.test/mcp","headers":[]}),
        )
        .unwrap();
        let mut declarations = vec![declaration];
        let before = declarations.clone();
        assert_eq!(
            forward_http_with_builtin(&BTreeMap::new(), &mut declarations, true),
            Err(RuntimeError::Declarations)
        );
        assert_eq!(declarations, before);
        assert!(forward_http_with_builtin(&BTreeMap::new(), &mut declarations, false).is_ok());
    }
}
