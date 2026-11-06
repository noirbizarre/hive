//! End-to-end agent startup, aggregate routing and direct HTTP forwarding.

use agent_client_protocol::{
    Client, Error,
    schema::{ProtocolVersion, v2},
};
use agent_client_protocol_http::HttpClient;
use hive_daemon::{Broker, config::Config};
use serde_json::{Value, json};
use std::{net::SocketAddr, path::Path, time::Duration};

fn config(directory: &Path, shared: bool, mode: &str) -> Config {
    let mut config: Config = serde_json::from_value(json!({
        "default_agent":"test", "agents":{"test":{
            "command":env!("CARGO_BIN_EXE_hive-test-mcp-agent"),
            "env":{"HIVE_MCP_REPORT":directory.join("report.json")},"mcp_servers":["a","b"]
        }}, "mcp":{"servers":{}}
    }))
    .unwrap();
    for id in ["a", "b"] {
        let mut definition = json!({"command":[env!("CARGO_BIN_EXE_hive-test-mcp"),directory.join(format!("{id}.jsonl")),mode],"cwd":directory,"hive":{"scope":"machine","idle_timeout_seconds":0}});
        if shared {
            definition["hive"]["sharing"] = json!({"mode":"shared","principal":"operator","partition":"test","policy_generation":"1","external_generations":{}});
        }
        config
            .mcp
            .servers
            .insert(id.into(), serde_json::from_value(definition).unwrap());
    }
    config.validate().unwrap();
    config
}

async fn serve(broker: Broker) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    (
        address,
        tokio::spawn(async move {
            axum::serve(listener, hive_daemon::router(broker))
                .await
                .unwrap()
        }),
    )
}

async fn create(
    address: SocketAddr,
    scheme: &str,
    directory: &Path,
) -> Result<v2::NewSessionResponse, Error> {
    let directory = directory.to_owned();
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::time::timeout(
        Duration::from_secs(30),
        Client.v2().connect_with(
            HttpClient::new(format!("{scheme}://{address}")).unwrap(),
            async move |cx| {
                cx.send_request(v2::InitializeRequest::new(
                    ProtocolVersion::V2,
                    v2::Implementation::new("mcp-route-test", "1"),
                ))
                .block_task()
                .await?;
                let result = cx
                    .send_request(v2::NewSessionRequest::new(directory))
                    .block_task()
                    .await;
                let _ = tx.send(result);
                Ok(())
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    rx.await.unwrap()
}

fn report(directory: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(directory.join("report.json")).unwrap()).unwrap()
}
fn events(directory: &Path, id: &str) -> Vec<Value> {
    std::fs::read_to_string(directory.join(format!("{id}.jsonl")))
        .unwrap_or_default()
        .split_inclusive('\n')
        // Polling can observe a writer's partial final record; only newline-complete events count.
        .filter(|line| line.ends_with('\n'))
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[tokio::test]
async fn built_in_workspace_tools_work_alone_and_beside_paginated_user_tools_during_setup() {
    for managed in [false, true] {
        for scheme in ["ws", "http"] {
            let temporary = tempfile::tempdir().unwrap();
            let directory = temporary.path().canonicalize().unwrap();
            let mut config = config(&directory, false, "routing");
            config.projects.insert(
                "demo".to_owned().try_into().unwrap(),
                serde_json::from_value(json!({"name":"Demo"})).unwrap(),
            );
            config.workspaces.insert(
                "task".into(),
                serde_json::from_value(json!({"policy":"existing","directory":directory})).unwrap(),
            );
            let agent = config.agents.get_mut("test").unwrap();
            agent.project = Some("demo".to_owned().try_into().unwrap());
            agent.workspace = Some("task".into());
            agent.env.insert("HIVE_WORKSPACE_TOOLS".into(), "1".into());
            if !managed {
                agent.mcp_servers.clear();
            }
            if managed {
                // A user's identically named tool remains addressable through its existing alias.
                for id in ["a", "b"] {
                    // Relative log arguments prove the copied declaration runs at the verified root.
                    config.mcp.servers.insert(id.into(), serde_json::from_value(json!({
                        "command":[env!("CARGO_BIN_EXE_hive-test-mcp"), format!("{id}.jsonl"), "routing"],
                        "cwd":".", "hive":{"idle_timeout_seconds":0}
                    })).unwrap());
                    std::fs::write(directory.join(format!("{id}.jsonl.builtin-name")), b"")
                        .unwrap();
                }
            }
            config.validate().unwrap();
            let broker =
                Broker::persistent(config, directory.join("state/catalog.sqlite3")).unwrap();
            let (address, server) = serve(broker.clone()).await;
            let opened = create(address, scheme, &directory).await.unwrap();
            let report = report(&directory);
            let context = &opened.meta.as_ref().unwrap()["hive"]["workspace"];
            let capabilities = &report["workspace"]["structuredContent"];
            assert_eq!(capabilities["workspace"], context["workspace"]);
            assert_eq!(capabilities["owner"], context["owner"]);
            assert_eq!(capabilities["project"], "demo");
            assert_eq!(capabilities["repositoryBackend"], Value::Null);
            assert_eq!(capabilities["provisioner"], "external.directory");
            assert_eq!(capabilities["schemaVersion"], 1);
            assert_eq!(
                capabilities["tools"],
                json!([{"name":"hive.workspace.capabilities","schemaVersion":1,"effect":"read_only"}])
            );
            assert_eq!(report["workspaceDirect"], report["workspace"]);
            assert_eq!(report["workspace"]["resultType"], "complete");
            let text: Value =
                serde_json::from_str(report["workspace"]["content"][0]["text"].as_str().unwrap())
                    .unwrap();
            assert_eq!(&text, capabilities);
            for key in ["workspaceArguments", "workspaceNull"] {
                assert_eq!(
                    report[key]["innerError"]["error"]["data"]["code"],
                    "hive::workspace_tools::arguments"
                );
            }
            assert_eq!(
                report["workspaceUnsupported"]["innerError"]["error"]["data"]["code"],
                "hive::workspace_tools::unsupported"
            );
            let tools = report["lists"]["tools"].as_array().unwrap();
            assert_eq!(tools.len(), if managed { 7 } else { 1 });
            assert_eq!(tools[0]["name"], "hive.workspace.capabilities");
            assert!(
                tools[1..]
                    .iter()
                    .all(|tool| tool["name"].as_str().unwrap().starts_with("hive_t_"))
            );
            if managed {
                assert!(
                    tools
                        .iter()
                        .any(|tool| tool["title"] == "a: hive.workspace.capabilities")
                );
            }
            assert_eq!(
                report["calls"].as_array().unwrap().len(),
                if managed { 4 } else { 0 }
            );
            assert!(!report.to_string().contains("backend-private-cursor"));
            broker.shutdown().await;
            server.abort();
        }
    }
}

#[tokio::test]
async fn automatic_workspace_tools_do_not_require_mcp_support_or_register_unmanaged_directories() {
    for registered in [false, true] {
        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path().canonicalize().unwrap();
        let mut config = config(&directory, false, "routing");
        let agent = config.agents.get_mut("test").unwrap();
        agent.mcp_servers.clear();
        if registered {
            agent.env.insert("HIVE_MCP_UNSUPPORTED".into(), "1".into());
            agent.project = Some("demo".to_owned().try_into().unwrap());
            agent.workspace = Some("task".into());
            config.projects.insert(
                "demo".to_owned().try_into().unwrap(),
                serde_json::from_value(json!({"name":"Demo"})).unwrap(),
            );
            config.workspaces.insert(
                "task".into(),
                serde_json::from_value(json!({"policy":"existing","directory":directory})).unwrap(),
            );
        }
        config.validate().unwrap();
        let broker = Broker::persistent(config, directory.join("state/catalog.sqlite3")).unwrap();
        let (address, server) = serve(broker.clone()).await;
        let opened = create(address, "ws", &directory).await.unwrap();
        assert!(
            report(&directory)["declarations"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        let workspace = &opened.meta.as_ref().unwrap()["hive"]["workspace"];
        assert_eq!(!workspace.is_null(), registered);
        broker.shutdown().await;
        server.abort();
    }
}

#[tokio::test]
async fn agents_discover_and_invoke_two_namespaced_backends_during_setup_on_both_transports() {
    for scheme in ["ws", "http"] {
        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path().canonicalize().unwrap();
        let mut config = config(&directory, true, "routing");
        let refresh = directory.join("refresh");
        config.agents.get_mut("test").unwrap().env.insert(
            "HIVE_MCP_REFRESH_TRIGGER".into(),
            refresh.display().to_string(),
        );
        let broker = Broker::new(config).unwrap();
        let (address, server) = serve(broker.clone()).await;
        create(address, scheme, &directory).await.unwrap();
        let first = report(&directory);
        assert_eq!(first["declarations"].as_array().unwrap().len(), 1);
        assert_eq!(first["declarations"][0]["type"], "acp");
        assert_eq!(
            first["discovery"]["supportedVersions"],
            json!(["2026-07-28"])
        );
        assert_eq!(
            first["discovery"]["capabilities"],
            json!({"tools":{},"resources":{},"prompts":{}})
        );
        let tools = first["lists"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 6);
        let names: std::collections::BTreeSet<_> =
            tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names.len(), 6);
        assert!(!first.to_string().contains("backend-private-cursor"));
        let calls = first["calls"].as_array().unwrap();
        for index in [0, 2] {
            assert_eq!(
                calls[index]["structuredContent"]["arguments"],
                json!({"value":"echo"})
            );
            assert_eq!(
                calls[index]["structuredContent"]["uri"],
                "application-native"
            );
            assert_eq!(calls[index]["content"][1]["text"], "file:///opaque");
            assert!(
                calls[index]["content"][0]["uri"]
                    .as_str()
                    .unwrap()
                    .starts_with("hive-mcp://")
            );
            assert!(!calls[index + 1].to_string().contains("seed-secret"));
        }
        assert_ne!(
            calls[0]["structuredContent"]["pid"],
            calls[2]["structuredContent"]["pid"]
        );
        assert_ne!(
            first["reads"][0]["contents"][0]["uri"],
            first["reads"][1]["contents"][0]["uri"]
        );
        assert_eq!(first["linkedReads"].as_array().unwrap().len(), 2);
        assert_eq!(first["templateReads"].as_array().unwrap().len(), 2);
        assert!(
            first["lists"]["resourceTemplates"][0]["uriTemplate"]
                .as_str()
                .unwrap()
                .ends_with("file:///{+path}{?revision}")
        );
        assert!(
            first["prompts"][0]["messages"][0]["content"]["resource"]["uri"]
                .as_str()
                .unwrap()
                .starts_with("hive-mcp://")
        );
        for error in ["unknown", "badCursor", "wrongMethod"] {
            assert!(first[error].get("innerError").is_some());
            assert_eq!(first[error]["innerError"]["error"]["code"], -32602);
        }
        assert_eq!(first["unsupported"]["innerError"]["error"]["code"], -32601);
        for id in ["a", "b"] {
            let log = events(&directory, id);
            assert!(log.iter().filter(|e| e["method"] == "tools/list").all(|e| {
                e["params"]["_meta"]
                    .get("io.modelcontextprotocol/protocolVersion")
                    .is_none()
            }));
            assert!(
                log.iter()
                    .filter(|e| e["method"] == "resources/read")
                    .all(|e| [
                        "file:///same?x=%2f#part",
                        "file:///unlisted",
                        "file:///expanded/path?revision=2"
                    ]
                    .iter()
                    .any(|uri| e["params"]["uri"] == *uri))
            );
        }
        // Trigger rediscovery only after setup completes, keeping the same downstream attachment.
        std::fs::write(directory.join("a.jsonl.changed"), b"").unwrap();
        std::fs::write(refresh, b"").unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            while report(&directory)["phase"] != "active" {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let second = report(&directory);
        assert_eq!(first["declarations"], second["declarations"]);
        assert_eq!(
            first["calls"][0]["structuredContent"]["pid"],
            second["calls"][0]["structuredContent"]["pid"]
        );
        assert_eq!(second["calls"][0]["resultType"], "complete");
        assert_ne!(
            first["lists"]["tools"][0]["name"],
            second["lists"]["tools"][0]["name"]
        );
        broker.shutdown().await;
        server.abort();
    }
}

#[tokio::test]
async fn only_explicitly_compatible_operator_assertions_share_configured_processes() {
    for shared in [false, true] {
        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path().canonicalize().unwrap();
        let broker = Broker::new(config(&directory, shared, "routing")).unwrap();
        let (address, server) = serve(broker.clone()).await;
        create(address, "ws", &directory).await.unwrap();
        create(address, "ws", &directory).await.unwrap();
        for id in ["a", "b"] {
            let log = events(&directory, id);
            assert_eq!(
                log.iter().filter(|e| e["event"] == "spawn").count(),
                if shared { 1 } else { 2 }
            );
            assert_eq!(
                log.iter().filter(|e| e["method"] == "initialize").count(),
                if shared { 1 } else { 2 }
            );
        }
        broker.shutdown().await;
        server.abort();
    }
}

#[tokio::test]
async fn unsupported_agents_start_no_backends_and_failed_initialization_publishes_no_session() {
    for failure in ["unsupported", "version", "reject"] {
        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path().canonicalize().unwrap();
        let mut config = config(
            &directory,
            false,
            if failure == "version" {
                "version"
            } else {
                "routing"
            },
        );
        if failure == "unsupported" {
            config
                .agents
                .get_mut("test")
                .unwrap()
                .env
                .insert("HIVE_MCP_UNSUPPORTED".into(), "1".into());
        }
        if failure == "reject" {
            config
                .agents
                .get_mut("test")
                .unwrap()
                .env
                .insert("HIVE_MCP_REJECT_SETUP".into(), "1".into());
        }
        let broker = Broker::new(config).unwrap();
        let (address, server) = serve(broker.clone()).await;
        let error = create(address, "ws", &directory).await.unwrap_err();
        if failure != "reject" {
            assert!(
                error
                    .data
                    .unwrap()
                    .get("code")
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .starts_with("hive::mcp::")
            );
        }
        if failure == "unsupported" {
            assert!(events(&directory, "a").is_empty());
        }
        if failure == "version" {
            assert!(!directory.join("report.json").exists());
        }
        let catalog: Value = reqwest::get(format!("http://{address}/v1/sessions"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(catalog["sessions"].as_array().unwrap().is_empty());
        broker.shutdown().await;
        server.abort();
    }
}

#[tokio::test]
async fn forwarded_http_remains_direct_alongside_the_managed_aggregate() {
    use axum::{Json, extract::State, http::HeaderMap, routing::post};
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/mcp", listener.local_addr().unwrap());
    let reference_value = std::env::var("PATH").unwrap();
    let http = axum::Router::new().route("/mcp", post(async |State(seen): State<std::sync::Arc<std::sync::Mutex<Vec<Value>>>>, headers: HeaderMap, Json(request): Json<Value>| {
        assert_eq!(headers["x-fixture"], "direct-secret");
        assert_eq!(headers["x-reference"], std::env::var("PATH").unwrap());
        seen.lock().unwrap().push(request.clone());
        Json(json!({"jsonrpc":"2.0","id":request["id"],"result":if request["method"] == "tools/list" {json!({"tools":[{"name":"remote","inputSchema":{"type":"object"}}]})} else {json!({"content":[{"type":"text","text":"remote called"}]})}}))
    })).with_state(seen.clone());
    let http_server = tokio::spawn(async move {
        axum::serve(listener, http).await.unwrap();
    });
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().canonicalize().unwrap();
    let mut config = config(&directory, true, "routing");
    config.mcp.servers.insert(
        "remote".into(),
        serde_json::from_value(json!({"url":endpoint,"headers":{"x-fixture":"direct-secret", "x-reference":{"env":"PATH"}}}))
            .unwrap(),
    );
    config
        .agents
        .get_mut("test")
        .unwrap()
        .mcp_servers
        .push("remote".into());
    let database = directory.join("state/catalog.sqlite3");
    let broker = Broker::persistent(config, database.clone()).unwrap();
    let (address, server) = serve(broker.clone()).await;
    create(address, "http", &directory).await.unwrap();
    let report = report(&directory);
    assert_eq!(report["declarations"][0]["type"], "http");
    assert_eq!(report["declarations"][0]["url"], endpoint);
    assert_eq!(
        report["declarations"][0]["headers"],
        json!([{"name":"x-fixture","value":"direct-secret"}, {"name":"x-reference","value":reference_value}])
    );
    assert_eq!(
        report["http"][0]["result"]["content"][0]["text"],
        "remote called"
    );
    assert_eq!(seen.lock().unwrap().len(), 2);
    let db = rusqlite::Connection::open(&database).unwrap();
    let record: String = db
        .query_row("SELECT record FROM sessions", [], |row| row.get(0))
        .unwrap();
    assert!(!record.contains("direct-secret"));
    assert!(!record.contains("x-reference"));
    broker.shutdown().await;
    server.abort();
    http_server.abort();
}

#[tokio::test]
async fn recovery_regenerates_the_aggregate_and_preserves_saved_configured_selectors() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().canonicalize().unwrap();
    let path = directory.join("state/catalog.sqlite3");
    let config = config(&directory, true, "routing");
    let broker = Broker::persistent(config.clone(), path.clone()).unwrap();
    let (address, server) = serve(broker.clone()).await;
    create(address, "ws", &directory).await.unwrap();
    let old = report(&directory);
    broker.shutdown().await;
    server.abort();
    drop(broker);
    let mut changed_profile = config;
    changed_profile
        .agents
        .get_mut("test")
        .unwrap()
        .mcp_servers
        .clear();
    let mut missing = changed_profile.clone();
    missing.mcp.servers.clear();
    let recovered = Broker::persistent(changed_profile, path.clone()).unwrap();
    tokio::join!(recovered.reconcile(), recovered.reconcile());
    let report = report(&directory);
    assert_eq!(report["phase"], "resume");
    assert_eq!(report["lists"]["tools"], old["lists"]["tools"]);
    assert_ne!(
        report["declarations"][0]["serverId"],
        old["declarations"][0]["serverId"]
    );
    for id in ["a", "b"] {
        assert_eq!(
            events(&directory, id)
                .iter()
                .filter(|e| e["event"] == "spawn")
                .count(),
            2
        );
    }
    recovered.shutdown().await;
    drop(recovered);
    std::fs::remove_file(directory.join("report.json")).unwrap();
    let unavailable = Broker::persistent(missing, path).unwrap();
    unavailable.reconcile().await;
    assert!(!directory.join("report.json").exists());
    let (address, server) = serve(unavailable.clone()).await;
    let catalog: Value = reqwest::get(format!("http://{address}/v1/sessions"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(catalog["sessions"][0]["recovery"], "unavailable");
    assert!(catalog.to_string().contains("hive::mcp::selection"));
    unavailable.shutdown().await;
    server.abort();
}

#[tokio::test]
async fn native_cancellation_keeps_setup_cleanup_owned_until_the_backend_settles() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().canonicalize().unwrap();
    let mut config = config(&directory, true, "routing");
    config.agents.get_mut("test").unwrap().env.insert(
        "HIVE_MCP_CANCEL_LOG".into(),
        directory.join("a.jsonl").display().to_string(),
    );
    let broker = Broker::new(config).unwrap();
    let (address, server) = serve(broker.clone()).await;
    let task_directory = directory.clone();
    let creation = tokio::spawn(async move { create(address, "ws", &task_directory).await });
    tokio::time::timeout(Duration::from_secs(10), async {
        while !events(&directory, "a")
            .iter()
            .any(|e| e["method"] == "notifications/cancelled")
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert!(!creation.is_finished());
    assert!(!directory.join("report.json").exists());
    assert!(
        !events(&directory, "b")
            .iter()
            .any(|e| e["method"] == "notifications/cancelled")
    );
    std::fs::write(directory.join("a.jsonl.release"), b"").unwrap();
    creation.await.unwrap().unwrap();
    assert_eq!(report(&directory)["cancelled"], true);
    broker.shutdown().await;
    server.abort();
}

#[tokio::test]
async fn same_daemon_recovery_keeps_private_bindings_and_pinned_definitions_across_reload() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().canonicalize().unwrap();
    let trigger = directory.join("exit-agent");
    let failed_resume = directory.join("fail-resume");
    let mut original = config(&directory, false, "routing");
    original.agents.get_mut("test").unwrap().env.insert(
        "HIVE_MCP_EXIT_TRIGGER".into(),
        trigger.display().to_string(),
    );
    let source_path = directory.join("config.toml");
    let source = hive_daemon::config::ConfigSource::new(Some(source_path.clone())).unwrap();
    let broker = Broker::persistent(original, directory.join("state/catalog.sqlite3"))
        .unwrap()
        .with_config_source(source);
    let (address, server) = serve(broker.clone()).await;
    create(address, "ws", &directory).await.unwrap();
    let initial = report(&directory);
    std::fs::write(&trigger, b"").unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let catalog: Value = reqwest::get(format!("http://{address}/v1/sessions"))
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            if catalog["sessions"][0]["recovery"] == "unavailable" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    std::fs::remove_file(&trigger).unwrap();
    // Remove definitions and selectors for new sessions while preserving the original worker's scope.
    std::fs::write(&source_path, format!("default_agent = 'test'\n[agents.test]\ncommand = '{}'\n[agents.test.env]\nHIVE_MCP_REPORT = '{}'\nHIVE_MCP_EXIT_TRIGGER = '{}'\nHIVE_MCP_FAIL_RESUME = '{}'\n",
        env!("CARGO_BIN_EXE_hive-test-mcp-agent"), directory.join("report.json").display(), trigger.display(), failed_resume.display())).unwrap();
    broker.reload_config().await.unwrap();
    std::fs::write(&failed_resume, b"").unwrap();
    broker.reconcile().await;
    assert_eq!(report(&directory)["phase"], "new");
    std::fs::remove_file(failed_resume).unwrap();
    broker.reconcile().await;
    let restored = report(&directory);
    assert_eq!(restored["phase"], "resume");
    assert_eq!(restored["lists"], initial["lists"]);
    for index in [0, 2] {
        assert_eq!(
            restored["calls"][index]["structuredContent"]["pid"],
            initial["calls"][index]["structuredContent"]["pid"]
        );
    }
    for id in ["a", "b"] {
        assert_eq!(
            events(&directory, id)
                .iter()
                .filter(|e| e["event"] == "spawn")
                .count(),
            1
        );
    }
    create(address, "http", &directory).await.unwrap();
    assert!(
        report(&directory)["declarations"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    broker.shutdown().await;
    server.abort();
}

#[tokio::test]
async fn malformed_backend_results_become_controlled_failures_without_replaying_invocations() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().canonicalize().unwrap();
    std::fs::write(directory.join("a.jsonl.malformed"), b"").unwrap();
    let broker = Broker::new(config(&directory, true, "routing")).unwrap();
    let (address, server) = serve(broker.clone()).await;
    create(address, "ws", &directory).await.unwrap();
    let report = report(&directory);
    assert!(
        report["calls"][0]["outerError"]
            .to_string()
            .contains("hive::mcp::response")
    );
    assert_eq!(report["calls"][2]["resultType"], "complete");
    assert_eq!(
        events(&directory, "a")
            .iter()
            .filter(|e| e["method"] == "tools/call" && e["params"]["name"] == "echo")
            .count(),
        1
    );
    broker.shutdown().await;
    server.abort();
}
