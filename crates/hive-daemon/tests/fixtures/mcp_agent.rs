//! Exercises MCP before responding to session setup, independently of Hive's route internals.

use agent_client_protocol::{Agent, Client, Error, Responder, Stdio, V2ConnectionTo, schema::v2};
use serde_json::{Value, json};

async fn invoke(
    cx: &V2ConnectionTo<Client>,
    server: &v2::McpServerAcpId,
    method: &str,
    mut params: Value,
) -> Result<Value, Error> {
    params["_meta"] = json!({"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}});
    let response = cx
        .send_request(
            v2::MessageMcpRequest::new(server.clone(), uuid::Uuid::new_v4().to_string(), method)
                .params(params.as_object().unwrap().clone()),
        )
        .block_task()
        .await?;
    match response {
        v2::MessageMcpResponse::Result { result, .. } => Ok(result),
        other => Ok(json!({"innerError":serde_json::to_value(other).unwrap()})),
    }
}

async fn exercise(
    cx: V2ConnectionTo<Client>,
    declarations: Vec<v2::McpServer>,
    phase: &str,
    cwd: Option<std::path::PathBuf>,
) -> Result<(), Error> {
    let mut report = json!({"phase":phase,"declarations":declarations,"lists":{},"calls":[],"reads":[],"linkedReads":[],"templateReads":[],"prompts":[],"http":[]});
    for declaration in &declarations {
        match declaration {
            v2::McpServer::Acp(server) => {
                let id = &server.server_id;
                report["discovery"] = invoke(&cx, id, "server/discover", json!({})).await?;
                if std::env::var_os("HIVE_WORKSPACE_TOOLS").is_some() {
                    // Stable built-ins must work before discovery has populated any alias table.
                    report["workspaceDirect"] = invoke(
                        &cx,
                        id,
                        "tools/call",
                        json!({"name":"hive.workspace.capabilities"}),
                    )
                    .await?;
                    for (key, name) in [
                        ("workspaceCurrent", "hive.workspace.current"),
                        ("workspaceInfo", "hive.workspace.info"),
                        ("workspaceList", "hive.workspace.list"),
                    ] {
                        report[key] =
                            invoke(&cx, id, "tools/call", json!({"name":name,"arguments":{}}))
                                .await?;
                    }
                    for (key, params) in [
                        (
                            "workspaceArguments",
                            json!({"name":"hive.workspace.capabilities","arguments":{"workspace":"other"}}),
                        ),
                        (
                            "workspaceNull",
                            json!({"name":"hive.workspace.capabilities","arguments":null}),
                        ),
                        (
                            "workspaceUnsupported",
                            json!({"name":"hive.vcs.unknown","arguments":{}}),
                        ),
                    ] {
                        report[key] = invoke(&cx, id, "tools/call", params).await?;
                    }
                }
                for (method, key) in [
                    ("tools/list", "tools"),
                    ("resources/list", "resources"),
                    ("resources/templates/list", "resourceTemplates"),
                    ("prompts/list", "prompts"),
                ] {
                    let mut params = json!({});
                    let mut items = Vec::new();
                    for _ in 0..128 {
                        let page = invoke(&cx, id, method, params.clone()).await?;
                        items.extend(
                            page[key]
                                .as_array()
                                .ok_or_else(Error::internal_error)?
                                .clone(),
                        );
                        if let Some(cursor) = page.get("nextCursor") {
                            report["lastCursor"] = cursor.clone();
                            params["cursor"] = cursor.clone();
                        } else {
                            break;
                        }
                    }
                    report["lists"][key] = json!(items);
                }
                for tool in report["lists"]["tools"].as_array().unwrap().clone() {
                    // Discovery alone must never cause generic fixture calls to record changes.
                    if tool["name"]
                        .as_str()
                        .is_some_and(|name| name.starts_with("hive.vcs."))
                    {
                        continue;
                    }
                    if tool["name"] == "hive.workspace.capabilities" {
                        report["workspace"] = invoke(
                            &cx,
                            id,
                            "tools/call",
                            json!({"name":tool["name"],"arguments":{}}),
                        )
                        .await?;
                        continue;
                    }
                    if tool["name"]
                        .as_str()
                        .is_some_and(|name| name.starts_with("hive.workspace."))
                    {
                        continue;
                    }
                    if tool["title"]
                        .as_str()
                        .is_some_and(|title| title.ends_with(": hold"))
                    {
                        if let Some(log) = std::env::var_os("HIVE_MCP_CANCEL_LOG") {
                            let prepared = cx.prepare_request(v2::MessageMcpRequest::new(id.clone(), uuid::Uuid::new_v4().to_string(), "tools/call")
                                .params(json!({"name":tool["name"],"arguments":{"content":[]},"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}).as_object().unwrap().clone()));
                            let cancel = prepared.cancellation_handle();
                            let response = prepared.block_task();
                            // Wait for actual publication to the backend, not a scheduler-dependent delay.
                            while !std::fs::read_to_string(&log)
                                .unwrap_or_default()
                                .contains("\"name\":\"hold\"")
                            {
                                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                            }
                            cancel.cancel()?;
                            report["cancelled"] = json!(response.await.is_err());
                            // Only one backend is targeted by this operation.
                            break;
                        }
                        continue;
                    }
                    let called = invoke(
                        &cx,
                        id,
                        "tools/call",
                        json!({"name":tool["name"],"arguments":{"value":"echo"}}),
                    )
                    .await;
                    report["calls"].as_array_mut().unwrap().push(match called {
                        Ok(v) => v,
                        Err(e) => json!({"outerError":e}),
                    });
                }
                if std::env::var_os("HIVE_VCS_EXERCISE").is_some() {
                    let current =
                        report["workspaceCurrent"]["structuredContent"]["workspace"].clone();
                    report["currentRemove"] = invoke(&cx, id, "tools/call", json!({"name":"hive.workspace.remove","arguments":{"workspace":current,"operation":format!("operation-{}",uuid::Uuid::new_v4())}})).await?;
                    let operation = format!("operation-{}", uuid::Uuid::new_v4());
                    let params =
                        json!({"name":"hive.workspace.create","arguments":{"operation":operation}});
                    report["siblingCreate"] = invoke(&cx, id, "tools/call", params.clone()).await?;
                    report["siblingReplay"] = invoke(&cx, id, "tools/call", params).await?;
                    let workspace =
                        report["siblingCreate"]["structuredContent"]["workspace"].clone();
                    report["siblingInfo"] = invoke(
                        &cx,
                        id,
                        "tools/call",
                        json!({"name":"hive.workspace.info","arguments":{"workspace":workspace}}),
                    )
                    .await?;
                    let cleanup = format!("operation-{}", uuid::Uuid::new_v4());
                    report["siblingRemove"] = invoke(&cx, id, "tools/call", json!({"name":"hive.workspace.remove","arguments":{"workspace":workspace,"operation":cleanup}})).await?;
                    report["siblingOperation"] = invoke(&cx, id, "tools/call", json!({"name":"hive.workspace.operation","arguments":{"operation":cleanup}})).await?;
                    let root = cwd.as_ref().ok_or_else(Error::internal_error)?;
                    std::fs::write(root.join("committed.txt"), format!("VCS tool {phase}\n"))
                        .map_err(|_| Error::internal_error())?;
                    std::fs::write(root.join("untracked.txt"), "must remain untracked\n")
                        .map_err(|_| Error::internal_error())?;
                    for (key, name, arguments) in [
                        ("vcsStatus", "hive.vcs.status", json!({})),
                        ("vcsDiff", "hive.vcs.diff", json!({})),
                        (
                            "vcsCommit",
                            "hive.vcs.commit",
                            json!({"message":format!("VCS tool {phase}"),"inclusion":"tracked_working_copy"}),
                        ),
                        ("vcsLog", "hive.vcs.log", json!({"limit":5})),
                    ] {
                        report[key] = invoke(
                            &cx,
                            id,
                            "tools/call",
                            json!({"name":name,"arguments":arguments}),
                        )
                        .await?;
                    }
                }
                for resource in report["lists"]["resources"].as_array().unwrap().clone() {
                    let read =
                        invoke(&cx, id, "resources/read", json!({"uri":resource["uri"]})).await?;
                    report["reads"].as_array_mut().unwrap().push(read);
                }
                for uri in report["calls"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|call| call["content"][0]["uri"].as_str())
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
                {
                    let read = invoke(&cx, id, "resources/read", json!({"uri":uri})).await?;
                    report["linkedReads"].as_array_mut().unwrap().push(read);
                }
                for template in report["lists"]["resourceTemplates"]
                    .as_array()
                    .unwrap()
                    .clone()
                {
                    let uri = template["uriTemplate"]
                        .as_str()
                        .unwrap()
                        .replace("{+path}{?revision}", "expanded/path?revision=2");
                    let read = invoke(&cx, id, "resources/read", json!({"uri":uri})).await?;
                    report["templateReads"].as_array_mut().unwrap().push(read);
                }
                for prompt in report["lists"]["prompts"].as_array().unwrap().clone() {
                    let result =
                        invoke(&cx, id, "prompts/get", json!({"name":prompt["name"]})).await?;
                    report["prompts"].as_array_mut().unwrap().push(result);
                }
                report["unknown"] = invoke(
                    &cx,
                    id,
                    "tools/call",
                    json!({"name":"unknown","arguments":{}}),
                )
                .await?;
                report["badCursor"] =
                    invoke(&cx, id, "tools/list", json!({"cursor":"foreign"})).await?;
                report["wrongMethod"] = invoke(
                    &cx,
                    id,
                    "resources/list",
                    json!({"cursor":report["lastCursor"]}),
                )
                .await?;
                report["unsupported"] = invoke(&cx, id, "not/supported", json!({})).await?;
            }
            v2::McpServer::Http(server) => {
                let client = reqwest::Client::new();
                let mut request = client
                    .post(&server.url)
                    .json(&json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}));
                for header in &server.headers {
                    request = request.header(&header.name, &header.value);
                }
                let response: Value = request
                    .send()
                    .await
                    .map_err(Error::into_internal_error)?
                    .json()
                    .await
                    .map_err(Error::into_internal_error)?;
                let name = response["result"]["tools"][0]["name"].clone();
                let mut request = client.post(&server.url).json(&json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":name,"arguments":{}}}));
                for header in &server.headers {
                    request = request.header(&header.name, &header.value);
                }
                let response: Value = request
                    .send()
                    .await
                    .map_err(Error::into_internal_error)?
                    .json()
                    .await
                    .map_err(Error::into_internal_error)?;
                report["http"].as_array_mut().unwrap().push(response);
            }
            _ => {}
        }
    }
    if let Some(path) = std::env::var_os("HIVE_MCP_REPORT") {
        let path = std::path::PathBuf::from(path);
        let temporary = path.with_extension("tmp");
        std::fs::write(&temporary, serde_json::to_vec(&report).unwrap())
            .map_err(Error::into_internal_error)?;
        // Active refresh is polled by another process; publish only complete report snapshots.
        std::fs::rename(temporary, path).map_err(Error::into_internal_error)?;
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let (declarations, snapshot) = tokio::sync::watch::channel(Vec::<v2::McpServer>::new());
    if let Some(trigger) = std::env::var_os("HIVE_MCP_EXIT_TRIGGER") {
        tokio::spawn(async move {
            loop {
                if std::path::Path::new(&trigger).exists() {
                    std::process::exit(9);
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        });
    }
    Agent
        .v2()
        .on_receive_request(
            async |request: v2::InitializeRequest,
                   responder: Responder<v2::InitializeResponse>,
                   _: V2ConnectionTo<Client>| {
                let mut capabilities = v2::SessionCapabilities::new();
                if std::env::var_os("HIVE_MCP_UNSUPPORTED").is_none() {
                    capabilities = capabilities.mcp(
                        v2::McpCapabilities::new()
                            .acp(v2::McpAcpCapabilities::new())
                            .http(v2::McpHttpCapabilities::new()),
                    );
                }
                responder.respond(
                    v2::InitializeResponse::new(
                        request.protocol_version,
                        v2::Implementation::new("mcp-test-agent", "1"),
                    )
                    .capabilities(v2::AgentCapabilities::new().session(capabilities)),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: v2::NewSessionRequest,
                        responder: Responder<v2::NewSessionResponse>,
                        cx: V2ConnectionTo<Client>| {
                let driver = cx.clone();
                let declarations = declarations.clone();
                cx.spawn(async move {
                    exercise(
                        driver,
                        request.mcp_servers.clone(),
                        "new",
                        Some(request.cwd.0.clone()),
                    )
                    .await?;
                    declarations.send_replace(request.mcp_servers);
                    if std::env::var_os("HIVE_MCP_REJECT_SETUP").is_some() {
                        return responder.respond_with_error(Error::internal_error());
                    }
                    responder.respond(v2::NewSessionResponse::new(v2::SessionId::new(
                        "mcp-session",
                    )))
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async |request: v2::ResumeSessionRequest,
                   responder: Responder<v2::ResumeSessionResponse>,
                   cx: V2ConnectionTo<Client>| {
                let driver = cx.clone();
                cx.spawn(async move {
                    if std::env::var_os("HIVE_MCP_FAIL_RESUME")
                        .is_some_and(|path| std::path::Path::new(&path).exists())
                    {
                        return responder.respond_with_error(Error::internal_error());
                    }
                    exercise(
                        driver.clone(),
                        request.mcp_servers,
                        "resume",
                        Some(request.cwd.0.clone()),
                    )
                    .await?;
                    driver.send_notification(v2::UpdateSessionNotification::new(
                        request.session_id,
                        v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(
                            v2::IdleStateUpdate::new(),
                        )),
                    ))?;
                    responder.respond(v2::ResumeSessionResponse::new())
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async |_: v2::CloseSessionRequest,
                   responder: Responder<v2::CloseSessionResponse>,
                   _: V2ConnectionTo<Client>| {
                responder.respond(v2::CloseSessionResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .with_spawned(move |cx: V2ConnectionTo<Client>| async move {
            if let Some(trigger) = std::env::var_os("HIVE_MCP_REFRESH_TRIGGER") {
                while !std::path::Path::new(&trigger).exists() {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
                let declarations = snapshot.borrow().clone();
                exercise(cx, declarations, "active", None).await?;
            }
            Ok(())
        })
        .connect_to(Stdio::new())
        .await
}
