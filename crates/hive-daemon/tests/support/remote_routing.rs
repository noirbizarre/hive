//! Two authoritative brokers prove that a gateway never becomes a second session owner.
use super::*;
use hive_core::identity::NodeId;
use hive_protocol::remote::{
    ListAgents, ListPeers, NewSession, Peer, ResolveSession, SessionReference,
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

fn registration(owner: NodeId, address: SocketAddr) -> hive_daemon::config::PeerRegistration {
    hive_daemon::config::PeerRegistration {
        owner,
        endpoint: format!("http://{address}/acp"),
        name: Some("Owner".into()),
        auth_token_env: None,
    }
}

async fn initialize(cx: V2ConnectionTo<Agent>) -> Result<Peer, Error> {
    Peer::initialize(
        cx,
        v2::InitializeRequest::new(
            ProtocolVersion::V2,
            v2::Implementation::new("routing-test", "1"),
        ),
    )
    .await
}

fn code(error: Error) -> Value {
    serde_json::to_value(error).unwrap()["data"]["code"].clone()
}

#[tokio::test]
async fn lost_creation_and_prompt_responses_never_redispatch_work_when_the_gateway_restarts() {
    for method in ["session/new", "session/prompt"] {
        let temporary = tempfile::tempdir().unwrap();
        let owner = hive_daemon::Broker::persistent(
            config_with_agent(
                "mock",
                env!("CARGO_BIN_EXE_hive-test-agent"),
                AgentVersion::V2,
            ),
            temporary.path().join("owner/catalog.sqlite3"),
        )
        .unwrap();
        let node = owner.mcp_execution_node().await.unwrap();
        let (owner_address, owner_server) = serve(owner.clone()).await;
        let mut config = Config::default();
        config
            .peers
            .insert("owner".into(), registration(node.clone(), owner_address));
        let path = temporary.path().join("gateway/catalog.sqlite3");
        let gateway = hive_daemon::Broker::persistent(config.clone(), path.clone()).unwrap();
        let (address, server) = serve(gateway.clone()).await;
        let (results, mut received) = mpsc::unbounded_channel();
        use agent_client_protocol::{Channel, ConnectTo};
        let (channel, proxy) = Channel::duplex();
        let (remote, driver) = ConnectTo::<Client>::into_channel_and_future(
            hive_protocol::transport::Credentials::default()
                .peer(&format!("http://{address}/acp"))
                .unwrap(),
        );
        let matching_id = std::sync::Arc::new(std::sync::Mutex::new(None::<Value>));
        let outbound_id = matching_id.clone();
        let proxy_server = tokio::spawn(async move {
            let bridge = Channel::bridge_with_inspection(
                proxy,
                remote,
                move |message| {
                    let value = serde_json::to_value(message).unwrap();
                    if value["method"] == method {
                        *outbound_id.lock().unwrap() = Some(value["id"].clone());
                    }
                    Ok(())
                },
                move |message| {
                    let value = serde_json::to_value(message).unwrap();
                    if matching_id.lock().unwrap().as_ref() == Some(&value["id"]) {
                        results.send(value).unwrap();
                        // Break the transport before the committed response reaches the client SDK.
                        return Err(Error::internal_error());
                    }
                    Ok(())
                },
            );
            let _ = tokio::try_join!(driver.unwrap(), bridge);
        });
        let first_node = node.clone();
        let failed = timeout(
            Duration::from_secs(20),
            Client.v2().connect_with(channel, async move |cx| {
                let peer = initialize(cx.clone()).await?;
                let opened = peer
                    .start_at(
                        &first_node,
                        "mock",
                        v2::NewSessionRequest::new(std::env::current_dir().unwrap()),
                    )
                    .await?;
                cx.prepare_request(
                    v2::PromptRequest::new(opened.session_id, vec!["one execution".into()])
                        .meta(controlled(&opened.meta, "lost-key")),
                )
                .block_task()
                .await?;
                Ok(())
            }),
        )
        .await
        .unwrap();
        assert!(failed.is_err());
        let lost = timeout(Duration::from_secs(5), received.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(lost.get("result").is_some());
        assert_eq!(
            summaries(owner_address).await["sessions"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        gateway.shutdown().await;
        server.abort();
        proxy_server.abort();
        let restarted = hive_daemon::Broker::persistent(config, path).unwrap();
        let (address, server) = serve(restarted.clone()).await;
        timeout(
            Duration::from_secs(15),
            Client.v2().connect_with(
                HttpClient::new(format!("ws://{address}")).unwrap(),
                async move |cx| {
                    initialize(cx.clone()).await?;
                    let listed = cx
                        .prepare_request(v2::ListSessionsRequest::new())
                        .block_task()
                        .await?;
                    assert_eq!(listed.sessions.len(), 1);
                    let existing = &listed.sessions[0];
                    let pilot = cx
                        .prepare_request(v2::ResumeSessionRequest::new(
                            existing.session_id.clone(),
                            existing.cwd.clone(),
                        ))
                        .block_task()
                        .await?;
                    if method == "session/prompt" {
                        let receipt = cx
                            .prepare_request(
                                v2::PromptRequest::new(
                                    existing.session_id.clone(),
                                    vec!["one execution".into()],
                                )
                                .meta(controlled(&pilot.meta, "lost-key")),
                            )
                            .block_task()
                            .await?;
                        assert_eq!(
                            serde_json::to_value(receipt).unwrap()["messageId"],
                            lost["result"]["messageId"]
                        );
                    }
                    cx.prepare_request(
                        v2::CloseSessionRequest::new(existing.session_id.clone()).meta(pilot.meta),
                    )
                    .block_task()
                    .await?;
                    Ok(())
                },
            ),
        )
        .await
        .unwrap()
        .unwrap();
        restarted.shutdown().await;
        owner.shutdown().await;
        server.abort();
        owner_server.abort();
    }
}

#[tokio::test]
async fn remote_sessions_keep_their_owner_and_prompt_receipts_across_gateway_restart() {
    for version in [AgentVersion::V1, AgentVersion::V2] {
        for scheme in ["ws", "http"] {
            let temporary = tempfile::tempdir().unwrap();
            let cwd = temporary.path().canonicalize().unwrap();
            let command = format!(
                "{}{}",
                env!("CARGO_BIN_EXE_hive-test-agent"),
                if version == AgentVersion::V1 {
                    " v1"
                } else {
                    ""
                }
            );
            let owner = hive_daemon::Broker::persistent(
                config_with_agent("mock", &command, version),
                cwd.join("owner/catalog.sqlite3"),
            )
            .unwrap();
            let node = owner.mcp_execution_node().await.unwrap();
            let (owner_address, owner_server) = serve(owner.clone()).await;
            let mut config = Config::default();
            config.peers.insert(
                "configured-owner".into(),
                registration(node.clone(), owner_address),
            );
            let gateway_path = cwd.join("gateway/catalog.sqlite3");
            let gateway =
                hive_daemon::Broker::persistent(config.clone(), gateway_path.clone()).unwrap();
            let (address, server) = serve(gateway.clone()).await;
            let (created, receive) = oneshot::channel();
            let remote_node = node.clone();
            let directory = cwd.clone();
            timeout(Duration::from_secs(30), Client.v2().connect_with(HttpClient::new(format!("{scheme}://{address}")).unwrap(), async move |cx| {
                initialize(cx.clone()).await?;
                let peers = cx.prepare_request(ListPeers(json!({}))).block_task().await?;
                assert_eq!(peers["peers"][0]["owner"], remote_node.as_str());
                assert!(peers["peers"][0].get("endpoint").is_none());
                let inventory = cx.prepare_request(ListAgents(json!({"peer":remote_node}))).block_task().await?;
                assert_eq!(inventory.agents[0].id, "mock");
                assert!(summaries(owner_address).await["sessions"].as_array().unwrap().is_empty());
                let opened = cx.prepare_request(NewSession(json!({"cwd":directory,"_meta":{"hive":{"peer":remote_node,"agent":"mock"}}}))).block_task().await?;
                assert!(opened.session_id.to_string().starts_with("hive-route-"));
                let session: SessionReference = serde_json::from_value(opened.meta.as_ref().unwrap()["hive"]["session"].clone()).unwrap();
                assert_eq!(session.owner, remote_node);
                assert_ne!(session.id, opened.session_id.to_string());
                let request = v2::PromptRequest::new(opened.session_id.clone(), vec![v2::ContentBlock::Text(v2::TextContent::new("retained intent"))]).meta(controlled(&opened.meta, "stable-key"));
                let receipt = cx.prepare_request(request.clone()).block_task().await?;
                let retried = cx.prepare_request(request).block_task().await?;
                assert_eq!(retried.message_id, receipt.message_id);
                let owner_id = session.id.clone();
                Client.v2().connect_with(HttpClient::new(format!("ws://{owner_address}")).unwrap(), async move |owner_cx| {
                    initialize(owner_cx.clone()).await?;
                    owner_cx.prepare_request(v2::ResumeSessionRequest::new(v2::SessionId::new(owner_id), directory)).block_task().await?;
                    Ok(())
                }).await?;
                let stale = cx.prepare_request(v2::PromptRequest::new(opened.session_id.clone(), vec!["must not execute".into()]).meta(controlled(&opened.meta, "stale-key"))).block_task().await;
                assert!(stale.is_err());
                let listed = cx.prepare_request(v2::ListSessionsRequest::new()).block_task().await?;
                assert_eq!(listed.sessions.len(), 1);
                assert_eq!(listed.sessions[0].session_id, opened.session_id);
                let _ = created.send((session, opened.session_id, receipt.message_id));
                Ok(())
            })).await.unwrap().unwrap();
            let (session, alias, receipt) = receive.await.unwrap();
            assert_eq!(
                summaries(owner_address).await["sessions"]
                    .as_array()
                    .unwrap()
                    .len(),
                1
            );
            gateway.shutdown().await;
            server.abort();
            let restarted = hive_daemon::Broker::persistent(config, gateway_path).unwrap();
            let (address, server) = serve(restarted.clone()).await;
            timeout(
                Duration::from_secs(30),
                Client.v2().connect_with(
                    HttpClient::new(format!("{scheme}://{address}")).unwrap(),
                    async move |cx| {
                        let peer = initialize(cx.clone()).await?;
                        let location = peer.resolve(session.clone()).await?;
                        assert_eq!(location.session_id, alias.to_string());
                        assert_eq!(location.session, session);
                        let observer = cx
                            .prepare_request(
                                v2::ResumeSessionRequest::new(alias.clone(), location.cwd.clone())
                                    .meta(v2::Meta::from_iter([(
                                        "hive".into(),
                                        json!({"mode":"observe"}),
                                    )])),
                            )
                            .block_task()
                            .await?;
                        assert!(
                            observer.meta.as_ref().unwrap()["hive"]
                                .get("token")
                                .is_none()
                        );
                        let pilot = cx
                            .prepare_request(v2::ResumeSessionRequest::new(
                                alias.clone(),
                                location.cwd,
                            ))
                            .block_task()
                            .await?;
                        let retried = cx
                            .prepare_request(
                                v2::PromptRequest::new(
                                    alias.clone(),
                                    vec![v2::ContentBlock::Text(v2::TextContent::new(
                                        "retained intent",
                                    ))],
                                )
                                .meta(controlled(&pilot.meta, "stable-key")),
                            )
                            .block_task()
                            .await?;
                        assert_eq!(retried.message_id, receipt);
                        let error = cx
                            .prepare_request(
                                v2::PromptRequest::new(
                                    alias.clone(),
                                    vec![v2::ContentBlock::Text(v2::TextContent::new(
                                        "changed intent",
                                    ))],
                                )
                                .meta(controlled(&pilot.meta, "stable-key")),
                            )
                            .block_task()
                            .await
                            .unwrap_err();
                        assert_eq!(
                            serde_json::to_value(error).unwrap()["data"]["hive"]["code"],
                            "hive::prompt::key_conflict"
                        );
                        cx.prepare_request(v2::CloseSessionRequest::new(alias).meta(pilot.meta))
                            .block_task()
                            .await?;
                        Ok(())
                    },
                ),
            )
            .await
            .unwrap()
            .unwrap();
            assert!(
                summaries(owner_address).await["sessions"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
            restarted.shutdown().await;
            owner.shutdown().await;
            server.abort();
            owner_server.abort();
        }
    }
}

#[tokio::test]
async fn request_scoped_elicitation_translates_parent_ids_through_the_gateway_without_blocking_acknowledgements()
 {
    for scheme in ["ws", "http"] {
        let (owner_address, owner_server, agent, mut reports) =
            interactions::owner_fixture("request").await;
        let (sender, receive) = oneshot::channel();
        Client
            .v2()
            .connect_with(
                HttpClient::new(format!("ws://{owner_address}")).unwrap(),
                async move |cx| {
                    let peer = initialize(cx).await?;
                    let _ = sender.send(peer.node);
                    Ok(())
                },
            )
            .await
            .unwrap();
        let node = NodeId::try_from(receive.await.unwrap()).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config
            .peers
            .insert("owner".into(), registration(node.clone(), owner_address));
        let gateway =
            hive_daemon::Broker::persistent(config, directory.path().join("state/catalog.sqlite3"))
                .unwrap();
        let (address, server) = serve(gateway.clone()).await;
        let (questions, mut incoming) = mpsc::unbounded_channel();
        timeout(
            Duration::from_secs(15),
            Client
                .v2()
                .on_receive_request(
                    async move |request: v2::CreateElicitationRequest,
                                responder: Responder<v2::CreateElicitationResponse>,
                                _: V2ConnectionTo<Agent>| {
                        questions.send((request, responder)).unwrap();
                        Ok(())
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .connect_with(
                    HttpClient::new(format!("{scheme}://{address}")).unwrap(),
                    async move |cx| {
                        let peer = Peer::initialize(
                            cx.clone(),
                            v2::InitializeRequest::new(
                                ProtocolVersion::V2,
                                v2::Implementation::new("forms", "1"),
                            )
                            .capabilities(
                                v2::ClientCapabilities::new().elicitation(
                                    v2::ElicitationCapabilities::new()
                                        .form(v2::ElicitationFormCapabilities::new()),
                                ),
                            ),
                        )
                        .await?;
                        let opened = peer
                            .start_at(
                                &node,
                                "interactions",
                                v2::NewSessionRequest::new(std::env::current_dir().unwrap()),
                            )
                            .await?;
                        let prepared = cx.prepare_request(
                            v2::PromptRequest::new(opened.session_id.clone(), vec!["ask".into()])
                                .meta(controlled(&opened.meta, "ask-key")),
                        );
                        let expected = prepared.id().clone();
                        let pending = prepared.block_task();
                        let (question, responder) = incoming.recv().await.unwrap();
                        let v2::ElicitationScope::Request(scope) = question.scope() else {
                            panic!("request scope was lost");
                        };
                        assert_eq!(scope.request_id, expected);
                        responder.respond(v2::CreateElicitationResponse::new(
                            v2::ElicitationAction::Cancel,
                        ))?;
                        pending.await?;
                        assert_eq!(reports.recv().await.unwrap()["action"], "cancel");
                        cx.prepare_request(
                            v2::CloseSessionRequest::new(opened.session_id).meta(opened.meta),
                        )
                        .block_task()
                        .await?;
                        Ok(())
                    },
                ),
        )
        .await
        .unwrap()
        .unwrap();
        gateway.shutdown().await;
        server.abort();
        owner_server.abort();
        agent.abort();
    }
}

#[tokio::test]
async fn remote_profile_configuration_and_cancellation_preserve_the_owners_event_ordering_domain() {
    let temporary = tempfile::tempdir().unwrap();
    let mut config = config_with_agent(
        "mock",
        env!("CARGO_BIN_EXE_hive-test-agent"),
        AgentVersion::V2,
    );
    config.profiles.insert(
        "remote-profile".into(),
        serde_json::from_value(json!({"agents":["mock"],"default_agent":"mock"})).unwrap(),
    );
    let owner =
        hive_daemon::Broker::persistent(config, temporary.path().join("owner/catalog.sqlite3"))
            .unwrap();
    let node = owner.mcp_execution_node().await.unwrap();
    let (owner_address, owner_server) = serve(owner.clone()).await;
    let mut config = Config::default();
    config
        .peers
        .insert("owner".into(), registration(node.clone(), owner_address));
    let gateway =
        hive_daemon::Broker::persistent(config, temporary.path().join("gateway/catalog.sqlite3"))
            .unwrap();
    let (address, server) = serve(gateway.clone()).await;
    let (updates, mut routed) = mpsc::unbounded_channel();
    timeout(
        Duration::from_secs(20),
        Client
            .v2()
            .on_receive_notification(
                async move |update: v2::UpdateSessionNotification, _: V2ConnectionTo<Agent>| {
                    let _ = updates.send(update);
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(
                HttpClient::new(format!("http://{address}")).unwrap(),
                async move |cx| {
                    let peer = initialize(cx.clone()).await?;
                    let inventory = peer.list_profiles_at(&node, Default::default()).await?;
                    assert_eq!(inventory.profiles[0].id, "remote-profile");
                    let opened = peer
                        .start_profile_at(
                            &node,
                            "remote-profile",
                            None,
                            v2::NewSessionRequest::new(std::env::current_dir().unwrap()),
                        )
                        .await?;
                    let session: SessionReference = serde_json::from_value(
                        opened.meta.as_ref().unwrap()["hive"]["session"].clone(),
                    )
                    .unwrap();
                    let (events, mut authoritative) = mpsc::unbounded_channel();
                    Client
                        .v2()
                        .on_receive_notification(
                            async move |update: v2::UpdateSessionNotification,
                                        _: V2ConnectionTo<Agent>| {
                                let _ = events.send(update);
                                Ok(())
                            },
                            agent_client_protocol::on_receive_notification!(),
                        )
                        .connect_with(
                            HttpClient::new(format!("ws://{owner_address}")).unwrap(),
                            async move |owner_cx| {
                                initialize(owner_cx.clone()).await?;
                                let observer = owner_cx
                                    .prepare_request(
                                        v2::ResumeSessionRequest::new(
                                            v2::SessionId::new(session.id),
                                            std::env::current_dir().unwrap(),
                                        )
                                        .meta(
                                            v2::Meta::from_iter([(
                                                "hive".into(),
                                                json!({"mode":"observe"}),
                                            )]),
                                        ),
                                    )
                                    .block_task()
                                    .await?;
                                assert!(
                                    observer.meta.as_ref().unwrap()["hive"]
                                        .get("token")
                                        .is_none()
                                );
                                cx.prepare_request(
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
                                cx.prepare_request(
                                    v2::PromptRequest::new(
                                        opened.session_id.clone(),
                                        vec!["close-running".into()],
                                    )
                                    .meta(controlled(&opened.meta, "cancel-key")),
                                )
                                .block_task()
                                .await?;
                                let original = loop {
                                    let update = authoritative.recv().await.unwrap();
                                    if matches!(update.update, v2::SessionUpdate::UserMessage(_)) {
                                        break update;
                                    }
                                };
                                let forwarded = loop {
                                    let update = routed.recv().await.unwrap();
                                    if matches!(update.update, v2::SessionUpdate::UserMessage(_)) {
                                        break update;
                                    }
                                };
                                assert_eq!(forwarded.session_id, opened.session_id);
                                assert_eq!(forwarded.meta, original.meta);
                                assert_eq!(
                                    serde_json::to_value(forwarded.update).unwrap(),
                                    serde_json::to_value(original.update).unwrap()
                                );
                                cx.send_notification(
                                    v2::CancelSessionNotification::new(opened.session_id.clone())
                                        .meta(opened.meta.clone()),
                                )?;
                                loop {
                                    let update = routed.recv().await.unwrap();
                                    if let v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(
                                        idle,
                                    )) = update.update
                                    {
                                        assert_eq!(
                                            idle.stop_reason,
                                            Some(v2::StopReason::Cancelled)
                                        );
                                        break;
                                    }
                                }
                                cx.prepare_request(
                                    v2::CloseSessionRequest::new(opened.session_id)
                                        .meta(opened.meta),
                                )
                                .block_task()
                                .await?;
                                Ok(())
                            },
                        )
                        .await?;
                    Ok(())
                },
            ),
    )
    .await
    .unwrap()
    .unwrap();
    gateway.shutdown().await;
    owner.shutdown().await;
    server.abort();
    owner_server.abort();
}

#[tokio::test]
async fn cyclic_peer_registrations_do_not_recurse_and_removed_owners_leave_only_cached_routing_metadata()
 {
    let temporary = tempfile::tempdir().unwrap();
    let left_path = temporary.path().join("left/catalog.sqlite3");
    let right_path = temporary.path().join("right/catalog.sqlite3");
    let initial_left =
        hive_daemon::Broker::persistent(Config::default(), left_path.clone()).unwrap();
    let initial_right =
        hive_daemon::Broker::persistent(Config::default(), right_path.clone()).unwrap();
    let left_node = initial_left.mcp_execution_node().await.unwrap();
    let right_node = initial_right.mcp_execution_node().await.unwrap();
    initial_left.shutdown().await;
    initial_right.shutdown().await;
    let left_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let right_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let left_address = left_listener.local_addr().unwrap();
    let right_address = right_listener.local_addr().unwrap();
    let mut left_config = Config::default();
    left_config.peers.insert(
        "right".into(),
        registration(right_node.clone(), right_address),
    );
    let mut right_config = config_with_agent(
        "mock",
        env!("CARGO_BIN_EXE_hive-test-agent"),
        AgentVersion::V2,
    );
    right_config
        .peers
        .insert("left".into(), registration(left_node, left_address));
    let source = temporary.path().join("reload.toml");
    std::fs::write(&source, "").unwrap();
    let left = hive_daemon::Broker::persistent(left_config, left_path)
        .unwrap()
        .with_config_source(hive_daemon::config::ConfigSource::new(Some(source)).unwrap());
    let right = hive_daemon::Broker::persistent(right_config, right_path).unwrap();
    let left_router = hive_daemon::router(left.clone());
    let right_router = hive_daemon::router(right.clone());
    let left_server = tokio::spawn(async move {
        axum::serve(left_listener, left_router).await.unwrap();
    });
    let right_server = tokio::spawn(async move {
        axum::serve(right_listener, right_router).await.unwrap();
    });
    let reload = left.clone();
    timeout(
        Duration::from_secs(15),
        Client.v2().connect_with(
            HttpClient::new(format!("ws://{left_address}")).unwrap(),
            async move |cx| {
                let peer = initialize(cx.clone()).await?;
                let opened = peer
                    .start_at(
                        &right_node,
                        "mock",
                        v2::NewSessionRequest::new(std::env::current_dir().unwrap()),
                    )
                    .await?;
                let session: SessionReference = serde_json::from_value(
                    opened.meta.as_ref().unwrap()["hive"]["session"].clone(),
                )
                .unwrap();
                let list = cx
                    .prepare_request(v2::ListSessionsRequest::new())
                    .block_task()
                    .await?;
                assert_eq!(list.sessions.len(), 1);
                assert_eq!(list.sessions[0].session_id, opened.session_id);
                reload.reload_config().await.unwrap();
                assert_eq!(
                    code(peer.resolve(session).await.unwrap_err()),
                    "hive::remote::owner_unavailable"
                );
                let cached = cx
                    .prepare_request(v2::ListSessionsRequest::new())
                    .block_task()
                    .await?;
                assert_eq!(cached.sessions.len(), 1);
                assert_eq!(
                    cached.sessions[0].meta.as_ref().unwrap()["hive"]["cached"],
                    true
                );
                assert_eq!(cached.meta.as_ref().unwrap()["hive"]["partial"], true);
                // A registration reload does not revoke an already attached owner link or close its worker.
                cx.prepare_request(
                    v2::CloseSessionRequest::new(opened.session_id).meta(opened.meta),
                )
                .block_task()
                .await?;
                Ok(())
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    left.shutdown().await;
    right.shutdown().await;
    left_server.abort();
    right_server.abort();
}

#[tokio::test]
async fn unknown_sessions_and_unreachable_or_replaced_owners_are_distinct_without_disconnecting_the_client()
 {
    let temporary = tempfile::tempdir().unwrap();
    let owner = hive_daemon::Broker::persistent(
        Config::default(),
        temporary.path().join("owner/catalog.sqlite3"),
    )
    .unwrap();
    let node = owner.mcp_execution_node().await.unwrap();
    let (owner_address, owner_server) = serve(owner.clone()).await;
    let wrong_owner = NodeId::new();
    let unreachable_owner = NodeId::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let unreachable_address = listener.local_addr().unwrap();
    drop(listener);
    let mut config = Config::default();
    config
        .peers
        .insert("owner".into(), registration(node.clone(), owner_address));
    config.peers.insert(
        "replaced".into(),
        registration(wrong_owner.clone(), owner_address),
    );
    config.peers.insert(
        "unreachable".into(),
        registration(unreachable_owner.clone(), unreachable_address),
    );
    let gateway =
        hive_daemon::Broker::persistent(config, temporary.path().join("gateway/catalog.sqlite3"))
            .unwrap();
    let (address, server) = serve(gateway.clone()).await;
    timeout(
        Duration::from_secs(15),
        Client.v2().connect_with(
            HttpClient::new(format!("ws://{address}")).unwrap(),
            async move |cx| {
                initialize(cx.clone()).await?;
                for (owner, expected) in [
                    (node, "session_not_found"),
                    (wrong_owner, "stale_route"),
                    (unreachable_owner, "owner_unavailable"),
                    (NodeId::new(), "owner_unavailable"),
                ] {
                    let error = cx
                        .prepare_request(ResolveSession(
                            json!({"session":{"owner":owner,"id":"absent"}}),
                        ))
                        .block_task()
                        .await
                        .unwrap_err();
                    assert_eq!(code(error), format!("hive::remote::{expected}"));
                }
                let invalid = cx
                    .prepare_request(NewSession(
                        json!({"cwd": "/", "_meta":{"hive":{"peer":NodeId::new(),"agent":"mock"}}}),
                    ))
                    .block_task()
                    .await
                    .unwrap_err();
                assert_eq!(code(invalid), "hive::remote::owner_unavailable");
                // A failed launch on one peer must not poison the upstream SDK task actor.
                cx.prepare_request(ListPeers(json!({})))
                    .block_task()
                    .await?;
                Ok(())
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    gateway.shutdown().await;
    owner.shutdown().await;
    server.abort();
    owner_server.abort();
}

#[tokio::test]
async fn a_retained_session_can_be_located_when_its_owner_is_reachable_but_its_agent_is_unavailable()
 {
    let temporary = tempfile::tempdir().unwrap();
    let path = temporary.path().join("owner/catalog.sqlite3");
    let owner = hive_daemon::Broker::persistent(
        config_with_agent(
            "mock",
            env!("CARGO_BIN_EXE_hive-test-agent"),
            AgentVersion::V2,
        ),
        path.clone(),
    )
    .unwrap();
    let (address, server) = serve(owner.clone()).await;
    let (sender, receive) = oneshot::channel();
    Client
        .v2()
        .connect_with(
            HttpClient::new(format!("ws://{address}")).unwrap(),
            async move |cx| {
                let peer = initialize(cx).await?;
                let opened = peer
                    .start(
                        "mock",
                        v2::NewSessionRequest::new(std::env::current_dir().unwrap()),
                    )
                    .await?;
                let session: SessionReference =
                    serde_json::from_value(opened.meta.unwrap()["hive"]["session"].clone())
                        .unwrap();
                let _ = sender.send(session);
                Ok(())
            },
        )
        .await
        .unwrap();
    let session = receive.await.unwrap();
    owner.shutdown().await;
    server.abort();
    // Retain the owner's database/identity, but do not restore or replace its worker.
    let owner = hive_daemon::Broker::persistent(Config::default(), path).unwrap();
    let (address, owner_server) = serve(owner.clone()).await;
    let mut config = Config::default();
    config
        .peers
        .insert("owner".into(), registration(session.owner.clone(), address));
    let gateway =
        hive_daemon::Broker::persistent(config, temporary.path().join("gateway/catalog.sqlite3"))
            .unwrap();
    let (address, server) = serve(gateway.clone()).await;
    timeout(
        Duration::from_secs(15),
        Client.v2().connect_with(
            HttpClient::new(format!("ws://{address}")).unwrap(),
            async move |cx| {
                let peer = initialize(cx.clone()).await?;
                let location = peer.resolve(session).await?;
                let error = cx
                    .prepare_request(v2::ResumeSessionRequest::new(
                        v2::SessionId::new(location.session_id),
                        location.cwd,
                    ))
                    .block_task()
                    .await
                    .unwrap_err();
                assert_eq!(code(error), "hive::remote::agent_unavailable");
                assert_eq!(
                    cx.prepare_request(v2::ListSessionsRequest::new())
                        .block_task()
                        .await?
                        .sessions
                        .len(),
                    1
                );
                Ok(())
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    gateway.shutdown().await;
    owner.shutdown().await;
    server.abort();
    owner_server.abort();
}
