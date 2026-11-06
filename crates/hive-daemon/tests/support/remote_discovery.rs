//! Owner-side extension conformance over the actual peer HTTP transport.
use super::*;
use agent_client_protocol::ConnectTo;
use hive_protocol::{
    remote::{Initialize, ListAgents, ListParams, NewSession, Peer},
    transport::Credentials,
};
use serde_json::{Value, json};

async fn serve(broker: hive_daemon::Broker) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = hive_daemon::router(broker);
    (
        address,
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        }),
    )
}

fn initialize(hive: Value) -> Initialize {
    Initialize(
        json!({"protocolVersion": 2, "info": {"name": "remote-test", "version": "1"}, "capabilities": {"_meta": {"hive": hive}}}),
    )
}

fn diagnostic(error: Error, code: &str) {
    let value = serde_json::to_value(error).unwrap();
    assert_eq!(value["data"]["code"], format!("hive::remote::{code}"));
    assert!(
        value["data"]["help"]
            .as_str()
            .is_some_and(|help| !help.is_empty())
    );
    assert!(!value.to_string().contains("PRIVATE_SENTINEL"));
    assert_eq!(
        value["code"],
        if matches!(
            code,
            "invalid_params" | "invalid_workspace" | "unknown_agent"
        ) {
            -32602
        } else {
            -32060
        }
    );
}

#[tokio::test]
async fn a_peer_without_workspace_capability_never_receives_a_workspace_launch() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let requests = Arc::new(AtomicUsize::new(0));
    let observed = requests.clone();
    let node = hive_core::identity::NodeId::new().to_string();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, agent_client_protocol_http::AcpHttpServer::new(move || {
            let requests = requests.clone();
            let node = node.clone();
            Agent.v2()
                .on_receive_request(async move |_: Initialize, responder: Responder<Value>, _: V2ConnectionTo<Client>| {
                    // A selection-capable older owner must remain usable for plain ACP launches.
                    responder.respond(json!({"protocolVersion":2,"info":{"name":"old-owner","version":"1"},"capabilities":{"session":{},"_meta":{"hive":{"version":1,"node":node,"features":{"agents":{},"selection":{},"control":{}}}}}}))
                }, agent_client_protocol::on_receive_request!())
                .on_receive_request(async move |_: v2::NewSessionRequest, responder: Responder<v2::NewSessionResponse>, _: V2ConnectionTo<Client>| {
                    requests.fetch_add(1, Ordering::SeqCst);
                    responder.respond(v2::NewSessionResponse::new(v2::SessionId::new("plain")))
                }, agent_client_protocol::on_receive_request!())
        }).into_router()).await.unwrap();
    });
    Client
        .v2()
        .connect_with(
            Credentials::default()
                .peer(&format!("http://{address}/acp"))
                .unwrap(),
            async |cx| {
                let peer = Peer::initialize(
                    cx,
                    v2::InitializeRequest::new(
                        ProtocolVersion::V2,
                        v2::Implementation::new("peer", "1"),
                    ),
                )
                .await?;
                let mut request = v2::NewSessionRequest::new(std::env::current_dir().unwrap());
                request.meta = Some(v2::Meta::from_iter([(
                    "hive".into(),
                    json!({"workspace":{"kind":"new","project":"demo","policy":"task"}}),
                )]));
                diagnostic(
                    peer.start("coder", request).await.unwrap_err(),
                    "unsupported",
                );
                assert_eq!(observed.load(Ordering::SeqCst), 0);
                diagnostic(
                    peer.start_profile(
                        "personal",
                        None,
                        v2::NewSessionRequest::new(std::env::current_dir().unwrap()),
                    )
                    .await
                    .unwrap_err(),
                    "unsupported",
                );
                let request = v2::NewSessionRequest::new(std::env::current_dir().unwrap()).meta(
                    v2::Meta::from_iter([("hive".into(), json!({"profile":"personal"}))]),
                );
                diagnostic(
                    peer.start("coder", request).await.unwrap_err(),
                    "unsupported",
                );
                assert_eq!(observed.load(Ordering::SeqCst), 0);
                peer.start(
                    "coder",
                    v2::NewSessionRequest::new(std::env::current_dir().unwrap()),
                )
                .await?;
                assert_eq!(observed.load(Ordering::SeqCst), 1);
                Ok(())
            },
        )
        .await
        .unwrap();
    server.abort();
}

#[tokio::test]
async fn discovery_is_safe_paginated_and_keeps_one_snapshot_through_reload() {
    let temporary = tempfile::tempdir().unwrap();
    let mut config = config_with_agent(
        "z-default",
        env!("CARGO_BIN_EXE_hive-test-agent"),
        AgentVersion::V2,
    );
    let mut first = config.agents["z-default"].clone();
    first.name = Some(String::new());
    first.args = vec!["PRIVATE_SENTINEL".into()];
    first
        .env
        .insert("PRIVATE_SENTINEL".into(), "PRIVATE_SENTINEL".into());
    first.versions = vec![AgentVersion::V2, AgentVersion::V2];
    config.agents.insert("a-first".into(), first);
    let marker = temporary.path().join("unexpected-start");
    for profile in config.agents.values_mut() {
        profile
            .env
            .insert("HIVE_MOCK_STARTED".into(), marker.to_str().unwrap().into());
    }
    let config_path = temporary.path().join("reload.toml");
    let broker =
        hive_daemon::Broker::persistent(config, temporary.path().join("state/catalog.sqlite3"))
            .unwrap()
            .with_config_source(
                hive_daemon::config::ConfigSource::new(Some(config_path.clone())).unwrap(),
            );
    let (address, server) = serve(broker.clone()).await;
    let reload = broker.clone();
    std::fs::write(
        &config_path,
        "[agents.replacement]\ncommand = 'PRIVATE_SENTINEL'\n",
    )
    .unwrap();
    Client
        .v2()
        .connect_with(
            Credentials::default()
                .peer(&format!("http://{address}/acp"))
                .unwrap(),
            async move |cx| {
                let peer = Peer::initialize(
                    cx.clone(),
                    v2::InitializeRequest::new(
                        ProtocolVersion::V2,
                        v2::Implementation::new("hive-peer", "1"),
                    ),
                )
                .await?;
                let first = peer
                    .list(ListParams {
                        limit: Some(1),
                        ..Default::default()
                    })
                    .await?;
                assert_eq!(first.default_agent.as_deref(), Some("z-default"));
                assert_eq!(first.agents[0].id, "a-first");
                assert_eq!(first.agents[0].name, "a-first");
                assert_eq!(first.agents[0].versions, vec![AgentVersion::V2]);
                assert_eq!(
                    serde_json::to_value(&first.agents[0].capabilities).unwrap(),
                    json!({})
                );
                assert!(
                    !serde_json::to_string(&first)
                        .unwrap()
                        .contains("PRIVATE_SENTINEL")
                );
                assert!(
                    summaries(address).await["sessions"]
                        .as_array()
                        .unwrap()
                        .is_empty()
                );
                for params in [
                    json!({"limit":0}),
                    json!({"limit":101}),
                    json!({"limit":1.5}),
                    json!({"cursor":""}),
                    json!({"cursor":"invalid"}),
                    json!({"command":"PRIVATE_SENTINEL"}),
                ] {
                    diagnostic(
                        cx.send_request(ListAgents(params))
                            .block_task()
                            .await
                            .unwrap_err(),
                        "invalid_params",
                    );
                }
                reload.reload_config().await.unwrap();
                let next = peer
                    .list(ListParams {
                        cursor: first.next_cursor.clone(),
                        ..Default::default()
                    })
                    .await?;
                assert_eq!(next.agents[0].id, "z-default");
                assert!(next.agents[0].is_default);
                assert!(next.next_cursor.is_none());
                diagnostic(
                    peer.list(ListParams {
                        cursor: first.next_cursor,
                        ..Default::default()
                    })
                    .await
                    .unwrap_err(),
                    "invalid_params",
                );
                diagnostic(
                    peer.start(
                        "a-first",
                        v2::NewSessionRequest::new(std::env::current_dir().unwrap()),
                    )
                    .await
                    .unwrap_err(),
                    "unknown_agent",
                );
                assert_eq!(
                    peer.list(ListParams::default()).await?.agents[0].id,
                    "replacement"
                );
                Ok(())
            },
        )
        .await
        .unwrap();
    assert!(
        !marker.exists(),
        "discovery must never probe or start an agent"
    );
    broker.shutdown().await;
    server.abort();
}

#[tokio::test]
async fn negotiation_rejects_malformed_offers_and_never_advertises_an_ephemeral_node() {
    let temporary = tempfile::tempdir().unwrap();
    let path = temporary.path().join("state/catalog.sqlite3");
    let mut identity = None;
    for persistent in [true, true, false] {
        let broker = if persistent {
            hive_daemon::Broker::persistent(Config::default(), path.clone()).unwrap()
        } else {
            hive_daemon::Broker::new(Config::default())
        };
        let (address, server) = serve(broker.clone()).await;
        for hive in [
            json!({"versions":[1,1]}),
            json!({"versions":[]}),
            json!({"versions":[0]}),
            json!({"versions":["1"]}),
            json!(false),
        ] {
            let (mut channel, driver) = ConnectTo::<Client>::into_channel_and_future(
                Credentials::default()
                    .peer(&format!("http://{address}/acp"))
                    .unwrap(),
            );
            let driver = tokio::spawn(driver.unwrap());
            let response = raw_exchange(
                &mut channel,
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":initialize(hive).0}),
            )
            .await;
            assert_eq!(
                response["error"]["data"]["code"],
                "hive::remote::invalid_params"
            );
            let retried = raw_exchange(&mut channel, json!({"jsonrpc":"2.0","id":2,"method":"initialize","params":initialize(json!({"versions":[2,1]})).0})).await;
            assert_eq!(retried["result"]["protocolVersion"], 2);
            let selected = &retried["result"]["capabilities"]["_meta"]["hive"];
            if persistent {
                assert_eq!(selected["version"], 1);
                assert!(selected["features"]["location"].is_null());
                if let Some(node) = &identity {
                    assert_eq!(&selected["node"], node);
                }
                identity = Some(selected["node"].clone());
            } else {
                assert!(selected.is_null());
            }
            driver.abort();
        }
        for offer in [Value::Null, json!({"versions":[99]})] {
            Client
                .v2()
                .connect_with(
                    Credentials::default()
                        .peer(&format!("http://{address}/acp"))
                        .unwrap(),
                    async move |cx| {
                        let result = cx
                            .send_request(initialize(offer.clone()))
                            .block_task()
                            .await?;
                        assert!(result["capabilities"]["_meta"]["hive"].is_null());
                        diagnostic(
                            cx.send_request(ListAgents(json!({})))
                                .block_task()
                                .await
                                .unwrap_err(),
                            "unsupported",
                        );
                        if !offer.is_null() {
                            diagnostic(
                                cx.send_request(NewSession(
                                    json!({"cwd":"/", "_meta":{"hive":{"agent":"missing"}}}),
                                ))
                                .block_task()
                                .await
                                .unwrap_err(),
                                "unsupported",
                            );
                        }
                        Ok(())
                    },
                )
                .await
                .unwrap();
        }
        broker.shutdown().await;
        server.abort();
        drop(broker);
        // Let aborted transport tasks drop their broker references before reopening the store.
        let _ = server.await;
    }
}

#[tokio::test]
async fn invalid_launch_inputs_have_no_execution_effects_and_spawn_failures_are_typed() {
    let temporary = tempfile::tempdir().unwrap();
    let cwd = temporary.path().canonicalize().unwrap();
    let marker = cwd.join("started");
    let mut config = config_with_agent(
        "mock",
        env!("CARGO_BIN_EXE_hive-test-agent"),
        AgentVersion::V2,
    );
    config
        .agents
        .get_mut("mock")
        .unwrap()
        .env
        .insert("HIVE_MOCK_STARTED".into(), marker.to_str().unwrap().into());
    let mut broken = config.agents["mock"].clone();
    broken.command = Some(cwd.join("PRIVATE_SENTINEL").display().to_string());
    config.agents.insert("broken".into(), broken);
    let broker =
        hive_daemon::Broker::persistent(config, cwd.join("state/catalog.sqlite3")).unwrap();
    let (address, server) = serve(broker.clone()).await;
    let check_marker = marker.clone();
    Client.v2().connect_with(Credentials::default().peer(&format!("http://{address}/acp")).unwrap(), async move |cx| {
        cx.send_request(initialize(json!({"versions":[1]}))).block_task().await?;
        let mut cases = vec![
            (json!({"cwd":cwd,"_meta":false}), "invalid_params"),
            (json!({"cwd":cwd,"_meta":{"hive":false}}), "invalid_params"),
            (json!({"cwd":cwd,"_meta":{"hive":{"agent":0}}}), "invalid_params"),
            (json!({"cwd":cwd,"_meta":{"hive":{"agent":""}}}), "invalid_params"),
            (json!({"cwd":cwd,"_meta":{"hive":{"agent":"mock","env":{"KEY":"PRIVATE_SENTINEL"}}}}), "invalid_params"),
            (json!({"cwd":cwd,"_meta":{"hive":{"agent":"PRIVATE_SENTINEL"}}}), "unknown_agent"),
            (json!({"cwd":"relative"}), "invalid_workspace"),
            (json!({"cwd":cwd.join("PRIVATE_SENTINEL")}), "invalid_workspace"),
            (json!({"cwd":cwd,"additionalDirectories":[cwd]}), "unsupported"),
            (json!({"cwd":cwd,"mcpServers":[{"type":"stdio","name":"secret","command":"PRIVATE_SENTINEL","args":[],"env":[]}]}), "unsupported"),
            (json!({"cwd":cwd,"_meta":{"hive":{"agent":"mock","workspace":{"kind":"existing","directory":"PRIVATE_SENTINEL"}}}}), "invalid_workspace"),
        ];
        let file = cwd.join("file");
        std::fs::write(&file, "file").unwrap();
        cases.push((json!({"cwd":file}), "invalid_workspace"));
        for (params, expected) in cases {
            diagnostic(cx.send_request(NewSession(params)).block_task().await.unwrap_err(), expected);
            assert!(!check_marker.exists());
            assert!(summaries(address).await["sessions"].as_array().unwrap().is_empty());
        }
        diagnostic(cx.send_request(NewSession(json!({"cwd":cwd,"_meta":{"hive":{"agent":"broken"}}}))).block_task().await.unwrap_err(), "launch_failed");
        assert!(summaries(address).await["sessions"].as_array().unwrap().is_empty());
        Ok(())
    }).await.unwrap();
    assert!(!marker.exists());
    broker.shutdown().await;
    server.abort();
}

#[tokio::test]
async fn startup_response_loss_retains_owner_evidence_and_never_retries_creation() {
    for fault in ["lose-response", "error-after-create"] {
        let temporary = tempfile::tempdir().unwrap();
        let cwd = temporary.path().canonicalize().unwrap();
        let state = cwd.join("agent.json");
        let mut config = config_with_agent(
            "mock",
            env!("CARGO_BIN_EXE_hive-test-agent"),
            AgentVersion::V2,
        );
        let profile = config.agents.get_mut("mock").unwrap();
        profile
            .env
            .insert("HIVE_MOCK_STATE".into(), state.to_str().unwrap().into());
        profile.env.insert("HIVE_MOCK_STARTUP".into(), fault.into());
        let broker =
            hive_daemon::Broker::persistent(config, cwd.join("state/catalog.sqlite3")).unwrap();
        let (address, server) = serve(broker.clone()).await;
        Client
            .v2()
            .connect_with(
                Credentials::default()
                    .peer(&format!("http://{address}/acp"))
                    .unwrap(),
                async move |cx| {
                    let peer = Peer::initialize(
                        cx,
                        v2::InitializeRequest::new(
                            ProtocolVersion::V2,
                            v2::Implementation::new("hive-peer", "1"),
                        ),
                    )
                    .await?;
                    diagnostic(
                        peer.start("mock", v2::NewSessionRequest::new(cwd))
                            .await
                            .unwrap_err(),
                        "launch_uncertain",
                    );
                    let inventory = summaries(address).await;
                    assert_eq!(inventory["sessions"].as_array().unwrap().len(), 1);
                    assert_eq!(inventory["sessions"][0]["lifecycle"], "unavailable");
                    Ok(())
                },
            )
            .await
            .unwrap();
        let evidence: Value = serde_json::from_slice(&std::fs::read(state).unwrap()).unwrap();
        assert_eq!(evidence["next"], 1);
        assert_eq!(evidence["sessions"].as_object().unwrap().len(), 1);
        broker.shutdown().await;
        server.abort();
    }
}

#[tokio::test]
async fn selected_nondefault_agents_keep_owner_cwd_and_survive_peer_disconnect_on_both_adapters() {
    for version in [AgentVersion::V1, AgentVersion::V2] {
        let temporary = tempfile::tempdir().unwrap();
        let cwd = temporary.path().canonicalize().unwrap();
        let state = cwd.join("agent.json");
        let mut config =
            config_with_agent("default", "nonexistent-default-agent", AgentVersion::V2);
        let mut selected =
            config_with_agent("selected", env!("CARGO_BIN_EXE_hive-test-agent"), version)
                .agents
                .remove("selected")
                .unwrap();
        if version == AgentVersion::V1 {
            selected.args.push("v1".into());
        }
        selected
            .env
            .insert("HIVE_MOCK_STATE".into(), state.to_str().unwrap().into());
        config.agents.insert("selected".into(), selected);
        let broker =
            hive_daemon::Broker::persistent(config, cwd.join("state/catalog.sqlite3")).unwrap();
        let (address, server) = serve(broker.clone()).await;
        let (tx, rx) = oneshot::channel();
        let directory = cwd.clone();
        Client
            .v2()
            .connect_with(
                Credentials::default()
                    .peer(&format!("http://{address}/acp"))
                    .unwrap(),
                async move |cx| {
                    let peer = Peer::initialize(
                        cx.clone(),
                        v2::InitializeRequest::new(
                            ProtocolVersion::V2,
                            v2::Implementation::new("hive-peer", "1"),
                        ),
                    )
                    .await?;
                    let opened = peer
                        .start("selected", v2::NewSessionRequest::new(directory))
                        .await?;
                    cx.send_request(
                        v2::SetSessionConfigOptionRequest::new(
                            opened.session_id.clone(),
                            v2::SessionConfigId::new("model"),
                            v2::SessionConfigOptionValue::Id {
                                value: v2::SessionConfigValueId::new("careful"),
                            },
                        )
                        .meta(opened.meta.clone()),
                    )
                    .block_task()
                    .await?;
                    tx.send(opened.session_id).unwrap();
                    Ok(())
                },
            )
            .await
            .unwrap();
        let id = rx.await.unwrap();
        let catalog = summaries(address).await;
        assert_eq!(catalog["sessions"].as_array().unwrap().len(), 1);
        assert_eq!(catalog["sessions"][0]["agent"], "selected");
        assert_eq!(catalog["sessions"][0]["cwd"].as_str(), cwd.to_str());
        let saved: Value = serde_json::from_slice(&std::fs::read(state).unwrap()).unwrap();
        let sessions = if version == AgentVersion::V1 {
            &saved["v1_sessions"]
        } else {
            &saved["sessions"]
        };
        assert_eq!(
            sessions.as_object().unwrap().values().next().unwrap()[0].as_str(),
            cwd.to_str()
        );
        Client
            .v2()
            .connect_with(
                Credentials::default()
                    .peer(&format!("http://{address}/acp"))
                    .unwrap(),
                async move |cx| {
                    let _peer = Peer::initialize(
                        cx.clone(),
                        v2::InitializeRequest::new(
                            ProtocolVersion::V2,
                            v2::Implementation::new("hive-peer", "1"),
                        ),
                    )
                    .await?;
                    let resumed = cx
                        .send_request(v2::ResumeSessionRequest::new(id.clone(), cwd))
                        .block_task()
                        .await?;
                    assert_eq!(
                        serde_json::to_value(&resumed.config_options[0]).unwrap()["currentValue"],
                        "careful"
                    );
                    cx.send_request(v2::CloseSessionRequest::new(id).meta(resumed.meta))
                        .block_task()
                        .await?;
                    assert!(
                        summaries(address).await["sessions"]
                            .as_array()
                            .unwrap()
                            .is_empty()
                    );
                    Ok(())
                },
            )
            .await
            .unwrap();
        broker.shutdown().await;
        server.abort();
    }
}
