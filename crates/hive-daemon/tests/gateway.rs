use std::{collections::BTreeMap, net::SocketAddr, time::Duration};

use agent_client_protocol::{
    Agent, Client, Error, Responder, V2ConnectionTo,
    schema::{ProtocolVersion, v2},
};
use agent_client_protocol_http::HttpClient;
use hive_daemon::config::{Config, Profile};
use hive_protocol::AgentVersion;
use tokio::{
    sync::{mpsc, oneshot},
    time::timeout,
};

#[path = "support/interactions.rs"]
mod interactions;

fn controlled(meta: &Option<v2::Meta>, key: &str) -> v2::Meta {
    let mut value = meta.clone().unwrap();
    value.get_mut("hive").unwrap()["key"] = serde_json::json!(key);
    value
}

async fn summaries(address: SocketAddr) -> serde_json::Value {
    reqwest::get(format!("http://{address}/v1/sessions"))
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap()
}

#[tokio::test]
async fn blocked_foreground_reserves_admission_until_idle_on_both_transports() {
    for mode in ["blocked-first", "running"] {
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
            .insert("HIVE_MOCK_FOREGROUND".into(), mode.into());
        let (address, server) = start_gateway(config).await;
        for scheme in ["ws", "http"] {
            let (tx, mut rx) = mpsc::unbounded_channel();
            timeout(
                Duration::from_secs(15),
                Client
                    .v2()
                    .on_receive_notification(
                        async move |update: v2::UpdateSessionNotification,
                                    _: V2ConnectionTo<Agent>| {
                            tx.send(update).map_err(Error::into_internal_error)
                        },
                        agent_client_protocol::on_receive_notification!(),
                    )
                    .connect_with(
                        HttpClient::new(format!("{scheme}://{address}")).unwrap(),
                        async move |cx| {
                            cx.send_request(v2::InitializeRequest::new(
                                ProtocolVersion::V2,
                                v2::Implementation::new("blocked-test", "1"),
                            ))
                            .block_task()
                            .await?;
                            let cwd = std::env::current_dir().unwrap();
                            let opened = cx
                                .send_request(v2::NewSessionRequest::new(cwd.clone()))
                                .block_task()
                                .await?;
                            let id = opened.session_id;
                            cx.send_request(
                                v2::SetSessionConfigOptionRequest::new(
                                    id.clone(),
                                    v2::SessionConfigId::new("model"),
                                    v2::SessionConfigOptionValue::Id {
                                        value: v2::SessionConfigValueId::new("fast"),
                                    },
                                )
                                .meta(opened.meta),
                            )
                            .block_task()
                            .await?;
                            let mut states = Vec::new();
                            loop {
                                let event = rx.recv().await.ok_or_else(Error::internal_error)?;
                                if let v2::SessionUpdate::StateUpdate(status) = &event.update {
                                    states.push(
                                        serde_json::to_value(status).unwrap()["state"]
                                            .as_str()
                                            .unwrap()
                                            .to_owned(),
                                    );
                                }
                                let value = serde_json::to_value(event.update).unwrap();
                                if value["sessionUpdate"] == "_hive_activity"
                                    && value["state"]["state"] == "requires_action"
                                {
                                    break;
                                }
                            }
                            assert_eq!(
                                states,
                                if mode == "running" {
                                    vec!["running", "requires_action"]
                                } else {
                                    vec!["requires_action"]
                                }
                            );
                            let attached = cx
                                .send_request(v2::ResumeSessionRequest::new(id.clone(), cwd))
                                .block_task()
                                .await?;
                            assert_blocked_admission(&cx, &id, &attached.meta).await?;
                            let catalog = summaries(address).await;
                            assert_eq!(
                                catalog["sessions"]
                                    .as_array()
                                    .unwrap()
                                    .iter()
                                    .find(|entry| entry["sessionId"] == id.to_string())
                                    .unwrap()["activity"],
                                "requires_action"
                            );
                            cx.send_notification(
                                v2::CancelSessionNotification::new(id.clone())
                                    .meta(attached.meta.clone()),
                            )?;
                            loop {
                                let event = rx.recv().await.ok_or_else(Error::internal_error)?;
                                let value = serde_json::to_value(event.update).unwrap();
                                if value["sessionUpdate"] == "_hive_activity"
                                    && value["state"]["state"] == "idle"
                                {
                                    break;
                                }
                            }
                            cx.send_request(
                                v2::SetSessionConfigOptionRequest::new(
                                    id.clone(),
                                    v2::SessionConfigId::new("model"),
                                    v2::SessionConfigOptionValue::Id {
                                        value: v2::SessionConfigValueId::new("careful"),
                                    },
                                )
                                .meta(attached.meta.clone()),
                            )
                            .block_task()
                            .await?;
                            let request = || {
                                v2::PromptRequest::new(
                                    id.clone(),
                                    vec!["resume-state-requires_action".into()],
                                )
                                .meta(controlled(&attached.meta, "accepted"))
                            };
                            let ack = cx.send_request(request()).block_task().await?;
                            loop {
                                let event = rx.recv().await.ok_or_else(Error::internal_error)?;
                                let value = serde_json::to_value(event.update).unwrap();
                                if value["sessionUpdate"] == "_hive_activity"
                                    && value["state"]["state"] == "requires_action"
                                {
                                    break;
                                }
                            }
                            assert_eq!(
                                cx.send_request(request()).block_task().await?.message_id,
                                ack.message_id
                            );
                            assert_blocked_admission(&cx, &id, &attached.meta).await?;
                            Ok::<_, Error>(())
                        },
                    ),
            )
            .await
            .unwrap()
            .unwrap();
        }
        server.abort();
    }
}

async fn assert_blocked_admission(
    cx: &V2ConnectionTo<Agent>,
    id: &v2::SessionId,
    meta: &Option<v2::Meta>,
) -> Result<(), Error> {
    let hive = &meta.as_ref().unwrap()["hive"];
    assert_eq!(hive["running"], true);
    assert_eq!(hive["activitySnapshot"]["foreground"], "requires_action");
    assert_ne!(
        hive["recoveryRequired"], true,
        "recovery must not mask foreground admission"
    );
    let error = cx
        .send_request(
            v2::PromptRequest::new(id.clone(), vec!["must not dispatch".into()])
                .meta(controlled(meta, "rejected")),
        )
        .block_task()
        .await
        .unwrap_err();
    assert!(
        error
            .data
            .unwrap()
            .to_string()
            .contains("active foreground")
    );
    assert!(
        cx.send_request(
            v2::SetSessionConfigOptionRequest::new(
                id.clone(),
                v2::SessionConfigId::new("model"),
                v2::SessionConfigOptionValue::Id {
                    value: v2::SessionConfigValueId::new("careful")
                }
            )
            .meta(meta.clone())
        )
        .block_task()
        .await
        .is_err()
    );
    assert!(
        cx.send_request(v2::CloseSessionRequest::new(id.clone()).meta(meta.clone()))
            .block_task()
            .await
            .is_err()
    );
    let listed = cx
        .send_request(v2::ListSessionsRequest::new())
        .block_task()
        .await?;
    let entry = listed
        .sessions
        .iter()
        .find(|entry| entry.session_id == *id)
        .unwrap();
    assert_eq!(entry.meta.as_ref().unwrap()["hive"]["running"], true);
    Ok(())
}

#[tokio::test]
async fn every_resume_replay_mode_ends_with_current_state_before_the_response_on_both_transports() {
    let config = config_with_agent(
        "mock",
        env!("CARGO_BIN_EXE_hive-test-agent"),
        AgentVersion::V2,
    );
    let (address, server) = start_gateway(config).await;
    for scheme in ["ws", "http"] {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let boundary = tx.clone();
        timeout(
            Duration::from_secs(30),
            Client
                .v2()
                .on_receive_notification(
                    async move |update: v2::UpdateSessionNotification, _: V2ConnectionTo<Agent>| {
                        tx.send(Some(update)).map_err(Error::into_internal_error)
                    },
                    agent_client_protocol::on_receive_notification!(),
                )
                .connect_with(
                    HttpClient::new(format!("{scheme}://{address}")).unwrap(),
                    async move |cx| {
                        cx.send_request(v2::InitializeRequest::new(
                            ProtocolVersion::V2,
                            v2::Implementation::new("barrier-test", "1"),
                        ))
                        .block_task()
                        .await?;
                        let cwd = std::env::current_dir().unwrap();
                        for status in ["unknown", "running", "idle", "requires_action"] {
                            let opened = cx
                                .send_request(v2::NewSessionRequest::new(cwd.clone()))
                                .block_task()
                                .await?;
                            let id = opened.session_id;
                            if status != "unknown" {
                                cx.send_request(
                                    v2::PromptRequest::new(
                                        id.clone(),
                                        vec![format!("resume-state-{status}").into()],
                                    )
                                    .meta(controlled(&opened.meta, status)),
                                )
                                .block_task()
                                .await?;
                                // The derived activity event follows the source state, closing ingestion deterministically.
                                loop {
                                    let event = rx
                                        .recv()
                                        .await
                                        .flatten()
                                        .ok_or_else(Error::internal_error)?;
                                    let value = serde_json::to_value(&event.update).unwrap();
                                    if value["sessionUpdate"] == "_hive_activity"
                                        && value["state"]["state"] == status
                                        && value["state"]["_meta"]["resumeSentinel"] == true
                                    {
                                        break;
                                    }
                                }
                            }
                            // Relinquish the creator so observe genuinely exercises a non-pilot attachment.
                            let mut meta = opened.meta.clone().unwrap();
                            meta.get_mut("hive").unwrap()["mode"] = "relinquish".into();
                            let (completed, response) = oneshot::channel();
                            let marker = boundary.clone();
                            cx.prepare_request(
                                v2::ResumeSessionRequest::new(id.clone(), cwd.clone()).meta(meta),
                            )
                            .on_receiving_result(
                                async move |result| {
                                    let _ = marker.send(None);
                                    let _ = completed.send(result);
                                    Ok(())
                                },
                            )?;
                            let relinquished =
                                response.await.map_err(|_| Error::internal_error())??;
                            let mut through = relinquished.meta.unwrap()["hive"]["nextSeq"]
                                .as_u64()
                                .unwrap()
                                - 1;
                            while rx.recv().await.ok_or_else(Error::internal_error)?.is_some() {}
                            for observe in [true, false] {
                                for replay in ["none", "start", "incremental"] {
                                    let mut request =
                                        v2::ResumeSessionRequest::new(id.clone(), cwd.clone());
                                    if observe {
                                        request = request.meta(v2::Meta::from_iter([(
                                            "hive".into(),
                                            serde_json::json!({"mode":"observe"}),
                                        )]));
                                    }
                                    request.replay_from = match replay {
                                        "start" => {
                                            Some(v2::ReplayFrom::Start(v2::ReplayFromStart::new()))
                                        }
                                        "incremental" => {
                                            Some(v2::ReplayFrom::Other(v2::OtherReplayFrom::new(
                                                "_hive_seq",
                                                BTreeMap::from([(
                                                    "seq".into(),
                                                    serde_json::json!(1),
                                                )]),
                                            )))
                                        }
                                        _ => None,
                                    };
                                    let (completed, response) = oneshot::channel();
                                    let marker = boundary.clone();
                                    cx.prepare_request(request).on_receiving_result(
                                        async move |result| {
                                            // The same ordered channel captures the actual response boundary.
                                            let _ = marker.send(None);
                                            let _ = completed.send(result);
                                            Ok(())
                                        },
                                    )?;
                                    let resumed =
                                        response.await.map_err(|_| Error::internal_error())??;
                                    let hive = &resumed.meta.as_ref().unwrap()["hive"];
                                    assert_eq!(
                                        hive["running"],
                                        matches!(status, "running" | "requires_action")
                                    );
                                    assert_eq!(hive["activitySnapshot"]["foreground"], status);
                                    assert_eq!(
                                        hive["role"],
                                        if observe { "observer" } else { "pilot" }
                                    );
                                    assert_eq!(hive.get("token").is_some(), !observe);
                                    if replay == "start" {
                                        assert_eq!(hive["snapshotThroughSeq"], through);
                                    }
                                    let mut batch = Vec::new();
                                    while let Some(event) =
                                        rx.recv().await.ok_or_else(Error::internal_error)?
                                    {
                                        assert_eq!(event.session_id, id);
                                        batch.push(event);
                                    }
                                    let fresh: Vec<_> = batch
                                        .iter()
                                        .filter(|event| {
                                            matches!(
                                                event.update,
                                                v2::SessionUpdate::StateUpdate(_)
                                            ) && event.meta.as_ref().unwrap()["hive"]["seq"]
                                                .as_u64()
                                                .unwrap()
                                                > through
                                        })
                                        .collect();
                                    if status == "unknown" {
                                        assert!(fresh.is_empty());
                                    } else {
                                        assert_eq!(
                                            fresh.len(),
                                            1,
                                            "{scheme}/{status}/{observe}/{replay}"
                                        );
                                        let barrier = fresh[0];
                                        assert!(std::ptr::eq(batch.last().unwrap(), barrier));
                                        let value = serde_json::to_value(&barrier.update).unwrap();
                                        assert_eq!(value["state"], status);
                                        assert_eq!(value["_meta"]["resumeSentinel"], true);
                                        assert_ne!(
                                            barrier.meta.as_ref().unwrap()["hive"]["snapshot"],
                                            true
                                        );
                                        assert_eq!(
                                            barrier.meta.as_ref().unwrap()["hive"]["seq"]
                                                .as_u64()
                                                .unwrap()
                                                + 1,
                                            hive["nextSeq"].as_u64().unwrap()
                                        );
                                        if replay == "start" {
                                            assert!(
                                                batch.iter().any(|event| event
                                                    .meta
                                                    .as_ref()
                                                    .unwrap()["hive"]["snapshot"]
                                                    == true)
                                            );
                                        }
                                        if replay == "incremental" {
                                            assert!(batch.iter().any(|event| {
                                                event.meta.as_ref().unwrap()["hive"]["seq"]
                                                    .as_u64()
                                                    .unwrap()
                                                    <= through
                                            }));
                                        }
                                    }
                                    // Every live allocation reaches existing subscribers, including observer-only resumes.
                                    let live: Vec<_> = batch
                                        .iter()
                                        .filter_map(|event| {
                                            event.meta.as_ref().unwrap()["hive"]["seq"].as_u64()
                                        })
                                        .filter(|seq| *seq > through)
                                        .collect();
                                    for (index, seq) in live.iter().enumerate() {
                                        assert_eq!(*seq, through + index as u64 + 1);
                                    }
                                    through = hive["nextSeq"].as_u64().unwrap() - 1;
                                }
                            }
                        }
                        Ok::<_, Error>(())
                    },
                ),
        )
        .await
        .unwrap()
        .unwrap();
    }
    server.abort();
}

#[tokio::test]
async fn reload_changes_new_sessions_without_interrupting_an_existing_worker() {
    let dir = std::env::temp_dir().join(format!("hive-live-reload-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&dir).unwrap();
    let path = dir.join("config.toml");
    let command = env!("CARGO_BIN_EXE_hive-test-agent");
    std::fs::write(
        &path,
        format!("default_agent = 'old'\n[agents.old]\ncommand = {command:?}\n"),
    )
    .unwrap();
    let source = hive_daemon::config::ConfigSource::new(Some(path.clone())).unwrap();
    let broker = hive_daemon::Broker::new(source.load().unwrap()).with_config_source(source);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router =
        hive_daemon::router_with_origin(broker.clone(), Some(&format!("http://{address}")));
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let connection = HttpClient::new(format!("ws://{address}")).unwrap();
    timeout(
        Duration::from_secs(15),
        Client.v2().connect_with(connection, async |cx| {
            cx.send_request(v2::InitializeRequest::new(
                ProtocolVersion::V2,
                v2::Implementation::new("reload-test", "1"),
            ))
            .block_task()
            .await?;
            let cwd = std::env::current_dir().map_err(Error::into_internal_error)?;
            let original = cx
                .send_request(v2::NewSessionRequest::new(cwd.clone()))
                .block_task()
                .await?;
            std::fs::write(
                &path,
                format!("default_agent = 'new'\n[agents.new]\ncommand = {command:?}\n"),
            )
            .unwrap();
            let client = reqwest::Client::new();
            let endpoint = format!("http://{address}/v1/config/reload");
            let before = summaries(address).await["revision"].as_u64().unwrap();
            let forbidden = client
                .post(&endpoint)
                .header("origin", "https://example.com")
                .send()
                .await
                .unwrap();
            assert_eq!(forbidden.status(), 403);
            assert_eq!(summaries(address).await["revision"], before);
            let reloaded: serde_json::Value = client
                .post(&endpoint)
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .json()
                .await
                .unwrap();
            assert!(reloaded["revision"].as_u64().unwrap() > before);
            let providers: serde_json::Value =
                reqwest::get(format!("http://{address}/v1/providers"))
                    .await
                    .unwrap()
                    .json()
                    .await
                    .unwrap();
            assert_eq!(providers["defaultAgent"], "new");
            assert_eq!(providers["agents"].as_array().unwrap().len(), 1);
            let new = cx
                .send_request(v2::NewSessionRequest::new(cwd.clone()))
                .block_task()
                .await?;
            assert_ne!(new.session_id, original.session_id);
            let list = summaries(address).await;
            assert!(
                list["sessions"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|s| s["agent"] == "old")
            );
            assert!(
                list["sessions"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|s| s["agent"] == "new")
            );
            // Both model setting and prompting exercise the retained original subprocess after removal.
            cx.send_request(
                v2::SetSessionConfigOptionRequest::new(
                    original.session_id.clone(),
                    v2::SessionConfigId::new("model"),
                    v2::SessionConfigOptionValue::Id {
                        value: v2::SessionConfigValueId::new("careful"),
                    },
                )
                .meta(original.meta.clone()),
            )
            .block_task()
            .await?;
            cx.send_request(
                v2::PromptRequest::new(original.session_id.clone(), vec!["still alive".into()])
                    .meta(controlled(&original.meta, "after-reload")),
            )
            .block_task()
            .await?;
            std::fs::write(&path, "unknown = 'secret-must-not-leak'\n[").unwrap();
            let revision = summaries(address).await["revision"].clone();
            let rejected = client.post(&endpoint).send().await.unwrap();
            assert_eq!(rejected.status(), 422);
            let error = rejected.text().await.unwrap();
            assert!(error.contains("hive::config::syntax"));
            assert!(!error.contains("secret-must-not-leak"));
            assert!(
                !broker
                    .reload_config()
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("secret-must-not-leak")
            );
            // Worker activity can advance the catalog independently; the selected default remains new.
            assert!(
                summaries(address).await["revision"].as_u64().unwrap()
                    >= revision.as_u64().unwrap()
            );
            Ok::<_, Error>(())
        }),
    )
    .await
    .unwrap()
    .unwrap();
    broker.shutdown().await;
    server.abort();
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn browser_origins_accept_localhost_on_the_listener_port_and_reject_other_origins() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let broker = hive_daemon::Broker::new(Config::default());
    let origin = format!("http://{address}");
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            hive_daemon::router_with_origin(broker, Some(&origin)),
        )
        .await
        .unwrap();
    });
    for (origin, expected) in [
        (Some(format!("http://{address}")), 101),
        (Some(format!("http://localhost:{}", address.port())), 101),
        (None, 101),
        (Some("http://localhost:1".into()), 403),
        (
            Some(format!("http://localhost.evil:{}", address.port())),
            403,
        ),
        (Some("https://example.com".into()), 403),
        (Some("null".into()), 403),
    ] {
        let mut request = reqwest::Client::new()
            .get(format!("http://{address}/acp"))
            .header("connection", "upgrade")
            .header("upgrade", "websocket")
            .header("sec-websocket-version", "13")
            .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==");
        if let Some(origin) = &origin {
            request = request.header("origin", origin);
        }
        let response = request.send().await.unwrap();
        assert_eq!(response.status().as_u16(), expected, "origin {origin:?}");
    }
    server.abort();
}

#[tokio::test]
async fn mode_notifications_after_the_setter_response_cannot_be_rolled_back() {
    for version in [AgentVersion::V1, AgentVersion::V2] {
        let command = format!(
            "{}{}",
            env!("CARGO_BIN_EXE_hive-test-agent"),
            if version == AgentVersion::V1 {
                " v1"
            } else {
                ""
            }
        );
        let mut config = config_with_agent("mock", &command, version);
        config
            .agents
            .get_mut("mock")
            .unwrap()
            .env
            .insert("HIVE_MOCK_SETTINGS".into(), "post-response".into());
        let (address, server) = start_gateway(config).await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        timeout(Duration::from_secs(15), Client.v2().on_receive_notification(
            async move |update: v2::UpdateSessionNotification, _: V2ConnectionTo<Agent>| { tx.send(update).map_err(Error::into_internal_error) },
            agent_client_protocol::on_receive_notification!(),
        ).connect_with(HttpClient::new(format!("ws://{address}")).unwrap(), async move |cx| {
            cx.send_request(v2::InitializeRequest::new(ProtocolVersion::V2, v2::Implementation::new("settings-order-test", "1"))).block_task().await?;
            let cwd = std::env::current_dir().map_err(Error::into_internal_error)?;
            let opened = cx.send_request(v2::NewSessionRequest::new(cwd.clone())).block_task().await?;
            cx.send_request(v2::SetSessionConfigOptionRequest::new(opened.session_id.clone(), v2::SessionConfigId::new("collaboration_mode"), v2::SessionConfigOptionValue::Id { value: v2::SessionConfigValueId::new("plan") }).meta(opened.meta)).block_task().await?;
            // The mock changes back immediately after replying, before the setter task can wake.
            let mut observed = Vec::new();
            while observed.len() < 2 {
                if let v2::SessionUpdate::ConfigOptionUpdate(update) = rx.recv().await.ok_or_else(Error::internal_error)?.update {
                    observed.push(serde_json::to_value(update.config_options).unwrap()[2]["currentValue"].clone());
                }
            }
            assert_eq!(observed, vec![serde_json::json!("plan"), serde_json::json!("default")]);
            let attached = cx.send_request(v2::ResumeSessionRequest::new(opened.session_id, cwd)).block_task().await?;
            assert_eq!(serde_json::to_value(attached.config_options).unwrap()[2]["currentValue"], "default");
            Ok::<_, Error>(())
        })).await.unwrap().unwrap();
        server.abort();
    }
}

#[tokio::test]
async fn modes_are_rediscovered_from_the_original_session_after_daemon_restart() {
    for (version, legacy) in [
        (AgentVersion::V1, true),
        (AgentVersion::V1, false),
        (AgentVersion::V2, false),
    ] {
        let dir = std::env::temp_dir().join(format!("hive-mode-recovery-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let database = dir.join("hive/sessions.db");
        let agent_state = dir.join("agent.json");
        let command = format!(
            "{}{}",
            env!("CARGO_BIN_EXE_hive-test-agent"),
            if version == AgentVersion::V1 {
                " v1"
            } else {
                ""
            }
        );
        let mut config = config_with_agent("mock", &command, version);
        let profile = config.agents.get_mut("mock").unwrap();
        profile
            .env
            .insert("HIVE_MOCK_STATE".into(), agent_state.display().to_string());
        profile.env.insert(
            if legacy {
                "HIVE_MOCK_MODES"
            } else {
                "HIVE_MOCK_SETTINGS"
            }
            .into(),
            "silent".into(),
        );
        let mut identity: Option<v2::SessionId> = None;
        for recovering in [false, true] {
            let broker = hive_daemon::Broker::persistent(config.clone(), database.clone()).unwrap();
            if recovering {
                broker.reconcile().await;
            }
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let router = hive_daemon::router(broker.clone());
            let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
            let saved_id = identity.clone();
            let id = timeout(
                Duration::from_secs(15),
                Client.v2().connect_with(
                    HttpClient::new(format!("ws://{address}")).unwrap(),
                    async move |cx| {
                        cx.send_request(v2::InitializeRequest::new(
                            ProtocolVersion::V2,
                            v2::Implementation::new("mode-recovery", "1"),
                        ))
                        .block_task()
                        .await?;
                        let cwd = std::env::current_dir().map_err(Error::into_internal_error)?;
                        if let Some(id) = saved_id {
                            let response = cx
                                .send_request(v2::ResumeSessionRequest::new(id.clone(), cwd))
                                .block_task()
                                .await?;
                            let options = serde_json::to_value(response.config_options).unwrap();
                            let mode = options
                                .as_array()
                                .unwrap()
                                .iter()
                                .find(|option| option["category"] == "mode")
                                .unwrap();
                            assert_eq!(mode["currentValue"], "plan");
                            Ok::<_, Error>(id)
                        } else {
                            let response = cx
                                .send_request(v2::NewSessionRequest::new(cwd))
                                .block_task()
                                .await?;
                            let mode = response
                                .config_options
                                .iter()
                                .find(|option| {
                                    serde_json::to_value(option).unwrap()["category"] == "mode"
                                })
                                .unwrap();
                            cx.send_request(
                                v2::SetSessionConfigOptionRequest::new(
                                    response.session_id.clone(),
                                    mode.config_id.clone(),
                                    v2::SessionConfigOptionValue::Id {
                                        value: v2::SessionConfigValueId::new("plan"),
                                    },
                                )
                                .meta(response.meta),
                            )
                            .block_task()
                            .await?;
                            Ok(response.session_id)
                        }
                    },
                ),
            )
            .await
            .unwrap()
            .unwrap();
            identity = Some(id);
            broker.shutdown().await;
            server.abort();
            // Windows cannot delete the database until cancellation releases the server's broker clone.
            let _ = server.await;
            drop(broker);
        }
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&agent_state).unwrap()).unwrap();
        assert_eq!(
            saved[if version == AgentVersion::V1 {
                "v1_sessions"
            } else {
                "sessions"
            }]
            .as_object()
            .unwrap()
            .len(),
            1
        );
        #[cfg(not(windows))]
        std::fs::remove_dir_all(dir).unwrap();
        #[cfg(windows)]
        {
            // Child-process and SQLite handles can outlive the test; the runner removes its unique temp directory.
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

#[tokio::test]
async fn native_modes_and_collaboration_settings_remain_independent_and_change_between_turns() {
    for version in [AgentVersion::V1, AgentVersion::V2] {
        let command = format!(
            "{}{}",
            env!("CARGO_BIN_EXE_hive-test-agent"),
            if version == AgentVersion::V1 {
                " v1"
            } else {
                ""
            }
        );
        let mut config = config_with_agent("mock", &command, version);
        config
            .agents
            .get_mut("mock")
            .unwrap()
            .env
            .insert("HIVE_MOCK_SETTINGS".into(), "1".into());
        let (address, server) = start_gateway(config).await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        timeout(
            Duration::from_secs(15),
            Client
                .v2()
                .on_receive_notification(
                    async move |update: v2::UpdateSessionNotification, _: V2ConnectionTo<Agent>| {
                        tx.send(update).map_err(Error::into_internal_error)
                    },
                    agent_client_protocol::on_receive_notification!(),
                )
                .connect_with(
                    HttpClient::new(format!("ws://{address}")).unwrap(),
                    async move |cx| {
                        cx.send_request(v2::InitializeRequest::new(
                            ProtocolVersion::V2,
                            v2::Implementation::new("native-settings-test", "1"),
                        ))
                        .block_task()
                        .await?;
                        let cwd = std::env::current_dir().map_err(Error::into_internal_error)?;
                        let opened = cx
                            .send_request(v2::NewSessionRequest::new(cwd.clone()))
                            .block_task()
                            .await?;
                        assert_eq!(opened.config_options.len(), 3);
                        cx.send_request(
                            v2::PromptRequest::new(
                                opened.session_id.clone(),
                                vec!["first turn".into()],
                            )
                            .meta(controlled(&opened.meta, "first")),
                        )
                        .block_task()
                        .await?;
                        while !matches!(
                            rx.recv().await.ok_or_else(Error::internal_error)?.update,
                            v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(_))
                        ) {}
                        let mut snapshot = opened.config_options.clone();
                        for (config, value) in [
                            ("model", "careful"),
                            ("collaboration_mode", "plan"),
                            ("mode", "plan"),
                        ] {
                            let setter = |value: &str| {
                                v2::SetSessionConfigOptionRequest::new(
                                    opened.session_id.clone(),
                                    v2::SessionConfigId::new(config),
                                    v2::SessionConfigOptionValue::Id {
                                        value: v2::SessionConfigValueId::new(value.to_owned()),
                                    },
                                )
                                .meta(opened.meta.clone())
                            };
                            assert!(
                                cx.send_request(setter("rejected"))
                                    .block_task()
                                    .await
                                    .is_err()
                            );
                            snapshot = cx
                                .send_request(setter(value))
                                .block_task()
                                .await?
                                .config_options;
                            let current = serde_json::to_value(&snapshot).unwrap();
                            assert_eq!(
                                current
                                    .as_array()
                                    .unwrap()
                                    .iter()
                                    .find(|option| option["configId"] == config)
                                    .unwrap()["currentValue"],
                                value
                            );
                        }
                        let current = serde_json::to_value(&snapshot).unwrap();
                        assert_eq!(current[0]["currentValue"], "careful");
                        assert_eq!(current[2]["category"], "collaboration_mode");
                        let (observer_tx, mut observer_rx) = mpsc::unbounded_channel();
                        Client.v2().on_receive_notification(
                            async move |update: v2::UpdateSessionNotification, _: V2ConnectionTo<Agent>| { observer_tx.send(update).map_err(Error::into_internal_error) },
                            agent_client_protocol::on_receive_notification!(),
                        ).connect_with(HttpClient::new(format!("http://{address}")).unwrap(), async |observer| {
                            observer.send_request(v2::InitializeRequest::new(ProtocolVersion::V2, v2::Implementation::new("settings-observer", "1"))).block_task().await?;
                            let mut meta = v2::Meta::new(); meta.insert("hive".into(), serde_json::json!({"mode":"observe"}));
                            observer.send_request(v2::ResumeSessionRequest::new(opened.session_id.clone(), cwd.clone()).meta(meta)).block_task().await?;
                            snapshot = cx.send_request(v2::SetSessionConfigOptionRequest::new(opened.session_id.clone(), v2::SessionConfigId::new("collaboration_mode"), v2::SessionConfigOptionValue::Id { value: v2::SessionConfigValueId::new("default") }).meta(opened.meta.clone())).block_task().await?.config_options;
                            // A response-only setter must still publish to another client and transport.
                            loop {
                                let update = observer_rx.recv().await.ok_or_else(Error::internal_error)?;
                                if let v2::SessionUpdate::ConfigOptionUpdate(update) = update.update
                                    && update.config_options == snapshot { break; }
                            }
                            Ok::<_, Error>(())
                        }).await?;
                        let attached = cx
                            .send_request(v2::ResumeSessionRequest::new(opened.session_id, cwd))
                            .block_task()
                            .await?;
                        assert_eq!(attached.config_options, snapshot);
                        Ok::<_, Error>(())
                    },
                ),
        )
        .await
        .unwrap()
        .unwrap();
        server.abort();
    }
}

#[tokio::test]
async fn legacy_modes_use_the_native_setter_and_reconcile_acknowledgements_and_observers() {
    for behavior in ["silent", "notify"] {
        for transport in ["ws", "http"] {
            let command = format!("{} v1", env!("CARGO_BIN_EXE_hive-test-agent"));
            let mut config = config_with_agent("mock", &command, AgentVersion::V1);
            config
                .agents
                .get_mut("mock")
                .unwrap()
                .env
                .insert("HIVE_MOCK_MODES".into(), behavior.into());
            let (address, server) = start_gateway(config).await;
            timeout(Duration::from_secs(15), Client.v2().connect_with(HttpClient::new(format!("{transport}://{address}")).unwrap(), async move |cx| {
                cx.send_request(v2::InitializeRequest::new(ProtocolVersion::V2, v2::Implementation::new("mode-test", "1"))).block_task().await?;
                let cwd = std::env::current_dir().map_err(Error::into_internal_error)?;
                let opened = cx.send_request(v2::NewSessionRequest::new(cwd.clone())).block_task().await?;
                assert_eq!(opened.config_options.len(), 1);
                let option = &opened.config_options[0];
                assert_eq!(serde_json::to_value(option).unwrap()["currentValue"], "build");
                let setter = |value: &str| v2::SetSessionConfigOptionRequest::new(opened.session_id.clone(), option.config_id.clone(), v2::SessionConfigOptionValue::Id { value: v2::SessionConfigValueId::new(value.to_owned()) }).meta(opened.meta.clone());
                assert!(cx.send_request(setter("unknown")).block_task().await.is_err());
                assert!(cx.send_request(setter("rejected")).block_task().await.is_err());
                let changed = cx.send_request(setter("plan")).block_task().await?;
                let expected = if behavior == "notify" { "build" } else { "plan" };
                assert_eq!(serde_json::to_value(&changed.config_options).unwrap()[0]["currentValue"], expected);
                let attached = cx.send_request(v2::ResumeSessionRequest::new(opened.session_id.clone(), cwd).meta({
                    let mut meta = opened.meta.clone().unwrap(); meta.get_mut("hive").unwrap()["mode"] = serde_json::json!("relinquish"); meta
                })).block_task().await?;
                assert_eq!(attached.config_options, changed.config_options);
                // The observing attachment cannot mutate even though it can see the current mode.
                let mut request = setter("build"); request.meta = attached.meta;
                assert!(cx.send_request(request).block_task().await.is_err());
                Ok::<_, Error>(())
            })).await.unwrap().unwrap();
            server.abort();
        }
    }
}

#[tokio::test]
async fn stop_reasons_and_native_failures_survive_the_gateway_and_replay() {
    for version in [AgentVersion::V1, AgentVersion::V2] {
        let command = format!(
            "{}{}",
            env!("CARGO_BIN_EXE_hive-test-agent"),
            if version == AgentVersion::V1 {
                " v1"
            } else {
                ""
            }
        );
        let (address, server) = start_gateway(config_with_agent("mock", &command, version)).await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let boundary_tx = tx.clone();
        timeout(
            Duration::from_secs(15),
            Client
                .v2()
                .on_receive_notification(
                    async move |update: v2::UpdateSessionNotification, _: V2ConnectionTo<Agent>| {
                        tx.send(Some(update)).map_err(Error::into_internal_error)
                    },
                    agent_client_protocol::on_receive_notification!(),
                )
                .connect_with(
                    HttpClient::new(format!("ws://{address}")).unwrap(),
                    async move |cx| {
                        cx.send_request(v2::InitializeRequest::new(
                            ProtocolVersion::V2,
                            v2::Implementation::new("stop-test", "1"),
                        ))
                        .block_task()
                        .await?;
                        let cwd = std::env::current_dir().map_err(Error::into_internal_error)?;
                        let opened = cx
                            .send_request(v2::NewSessionRequest::new(cwd.clone()))
                            .block_task()
                            .await?;
                        let id = opened.session_id;
                        if version == AgentVersion::V1 {
                            for (text, expected) in [
                                ("plain", "end_turn"),
                                ("stop-max-tokens", "max_tokens"),
                                ("stop-max-turn-requests", "max_turn_requests"),
                                ("stop-refusal", "refusal"),
                                ("wait-for-cancel", "cancelled"),
                                ("wait-for-cancel cancel-end-turn", "end_turn"),
                                ("wait-for-cancel cancel-error", "_v1_error"),
                            ] {
                                cx.send_request(
                                    v2::PromptRequest::new(id.clone(), vec![text.into()])
                                        .meta(controlled(&opened.meta, text)),
                                )
                                .block_task()
                                .await?;
                                loop {
                                    let event = rx
                                        .recv()
                                        .await
                                        .flatten()
                                        .ok_or_else(Error::internal_error)?;
                                    let value = serde_json::to_value(&event.update).unwrap();
                                    assert!(!value.to_string().contains("sk-must-not-leak"));
                                    if value["sessionUpdate"] == "agent_message_chunk"
                                        && value["content"]["text"] == "ready-for-cancel"
                                    {
                                        cx.send_notification(
                                            v2::CancelSessionNotification::new(id.clone())
                                                .meta(opened.meta.clone()),
                                        )?;
                                    }
                                    if value["sessionUpdate"] == "state_update"
                                        && value["state"] == "idle"
                                    {
                                        assert_eq!(value["stopReason"], expected);
                                        break;
                                    }
                                }
                            }
                        } else {
                            cx.send_request(
                                v2::PromptRequest::new(id.clone(), vec!["native-failure".into()])
                                    .meta(controlled(&opened.meta, "failure")),
                            )
                            .block_task()
                            .await?;
                            let mut saw_native = false;
                            let boundary = loop {
                                let event = rx
                                    .recv()
                                    .await
                                    .flatten()
                                    .ok_or_else(Error::internal_error)?;
                                let value = serde_json::to_value(&event.update).unwrap();
                                if value["sessionUpdate"] == "state_update"
                                    && value["state"] == "idle"
                                {
                                    saw_native = true;
                                    assert_eq!(value["stopReason"], "error");
                                }
                                if value["sessionUpdate"] == "_hive_error" {
                                    assert!(saw_native);
                                    assert_eq!(value["code"], "hive::agent::foreground_failed");
                                    assert!(!value.to_string().contains("sk-must-not-leak"));
                                    break event.meta.unwrap()["hive"]["seq"].as_u64().unwrap();
                                }
                            };
                            let (completed, response) = oneshot::channel();
                            let marker = boundary_tx.clone();
                            cx.prepare_request(
                                v2::ResumeSessionRequest::new(id.clone(), cwd.clone()).replay_from(
                                    v2::ReplayFrom::Other(v2::OtherReplayFrom::new(
                                        "_hive_seq",
                                        std::collections::BTreeMap::from([(
                                            "seq".into(),
                                            serde_json::json!(boundary),
                                        )]),
                                    )),
                                ),
                            )
                            .on_receiving_result(
                                async move |result| {
                                    let _ = marker.send(None);
                                    let _ = completed.send(result);
                                    Ok(())
                                },
                            )?;
                            response.await.map_err(|_| Error::internal_error())??;
                            let mut failures = 0;
                            while let Some(event) =
                                rx.recv().await.ok_or_else(Error::internal_error)?
                            {
                                let value = serde_json::to_value(event.update).unwrap();
                                if value["sessionUpdate"] == "_hive_error" {
                                    assert_eq!(event.meta.unwrap()["hive"]["seq"], boundary);
                                    failures += 1;
                                }
                            }
                            assert_eq!(failures, 1);
                            let (completed, response) = oneshot::channel();
                            cx.prepare_request(
                                v2::ResumeSessionRequest::new(id.clone(), cwd)
                                    .replay_from(v2::ReplayFrom::Start(v2::ReplayFromStart::new())),
                            )
                            .on_receiving_result(
                                async move |result| {
                                    // An ordered marker proves the complete snapshot batch has arrived.
                                    let _ = boundary_tx.send(None);
                                    let _ = completed.send(result);
                                    Ok(())
                                },
                            )?;
                            let replay = response.await.map_err(|_| Error::internal_error())??;
                            let through = replay.meta.unwrap()["hive"]["snapshotThroughSeq"]
                                .as_u64()
                                .unwrap();
                            let mut failures = 0;
                            // Snapshot delivery precedes the response, and can share its boundary cursor.
                            while let Some(event) =
                                rx.recv().await.ok_or_else(Error::internal_error)?
                            {
                                if event
                                    .meta
                                    .as_ref()
                                    .is_some_and(|meta| meta["hive"]["snapshot"] == true)
                                {
                                    let value = serde_json::to_value(event.update).unwrap();
                                    assert_ne!(value["sessionUpdate"], "state_update");
                                    assert!(!value.to_string().contains("sk-must-not-leak"));
                                    if value["sessionUpdate"] == "_hive_error" {
                                        failures += 1;
                                    }
                                }
                            }
                            assert_eq!(failures, 1);
                            assert!(through >= boundary);
                        }
                        Ok::<_, Error>(())
                    },
                ),
        )
        .await
        .unwrap()
        .unwrap();
        server.abort();
    }
}

#[tokio::test]
async fn model_choices_survive_translation_setters_rejection_and_observer_attachment() {
    for version in [AgentVersion::V1, AgentVersion::V2] {
        for transport in ["ws", "http"] {
            let command = format!(
                "{}{}",
                env!("CARGO_BIN_EXE_hive-test-agent"),
                if version == AgentVersion::V1 {
                    " v1"
                } else {
                    ""
                }
            );
            let (address, server) =
                start_gateway(config_with_agent("mock", &command, version)).await;
            let (tx, mut rx) = mpsc::unbounded_channel();
            let connection = HttpClient::new(format!("{transport}://{address}")).unwrap();
            timeout(Duration::from_secs(15), Client.v2().on_receive_notification(
                async move |update: v2::UpdateSessionNotification, _: V2ConnectionTo<Agent>| { tx.send(update).map_err(Error::into_internal_error) },
                agent_client_protocol::on_receive_notification!(),
            ).connect_with(connection, async move |cx| {
                cx.send_request(v2::InitializeRequest::new(ProtocolVersion::V2, v2::Implementation::new("model-test", "1"))).block_task().await?;
                let cwd = std::env::current_dir().map_err(Error::into_internal_error)?;
                let opened = cx.send_request(v2::NewSessionRequest::new(cwd.clone())).block_task().await?;
                assert_eq!(opened.config_options[0].config_id.to_string(), "model");
                let id = opened.session_id;
                let setter = |value: &str, meta: Option<v2::Meta>| v2::SetSessionConfigOptionRequest::new(id.clone(), v2::SessionConfigId::new("model"), v2::SessionConfigOptionValue::Id { value: v2::SessionConfigValueId::new(value.to_owned()) }).meta(meta);
                // The mock rejects a Hive ID or leaked token, verifying routing and credential stripping.
                for value in ["careful", "fast"] {
                    let response = cx.send_request(setter(value, opened.meta.clone())).block_task().await?;
                    assert_eq!(serde_json::to_value(&response.config_options).unwrap()[0]["currentValue"], value);
                }
                assert!(cx.send_request(setter("rejected", opened.meta.clone())).block_task().await.is_err());
                // A failed setter must release its reservation, permitting the next change immediately.
                let response = cx.send_request(setter("careful", opened.meta.clone())).block_task().await?;
                assert_eq!(serde_json::to_value(response.config_options).unwrap()[0]["currentValue"], "careful");
                let observer = cx.send_request(v2::ResumeSessionRequest::new(id.clone(), cwd.clone()).meta({
                    let mut meta = opened.meta.clone().unwrap(); meta.get_mut("hive").unwrap()["mode"] = serde_json::json!("relinquish"); meta
                })).block_task().await?;
                assert_eq!(serde_json::to_value(&observer.config_options).unwrap()[0]["currentValue"], "careful");
                assert!(cx.send_request(setter("fast", observer.meta)).block_task().await.is_err());
                let pilot = cx.send_request(v2::ResumeSessionRequest::new(id.clone(), cwd)).block_task().await?;
                assert!(cx.send_request(setter("fast", opened.meta)).block_task().await.is_err());
                cx.send_request(setter("fast", pilot.meta.clone())).block_task().await?;
                if version == AgentVersion::V1 {
                    let prompt = || v2::PromptRequest::new(id.clone(), vec!["fail-prompt".into()]).meta(controlled(&pilot.meta, "failed"));
                    cx.send_request(prompt()).block_task().await?;
                    let mut errors = 0;
                    loop {
                        let update = rx.recv().await.ok_or_else(Error::internal_error)?;
                        assert!(!serde_json::to_string(&update).unwrap().contains("sk-must-not-leak"));
                        match update.update {
                            v2::SessionUpdate::Other(other) if other.session_update == "_hive_error" => {
                                errors += 1;
                                assert_eq!(other.fields["code"], "hive::agent::prompt_failed");
                            }
                            v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(_)) => break,
                            _ => {}
                        }
                    }
                    assert_eq!(errors, 1);
                    cx.send_request(prompt()).block_task().await?;
                    assert_eq!(summaries(address).await["sessions"][0]["usage"]["turns"], 1);
                }
                Ok::<_, Error>(())
            })).await.unwrap().unwrap();
            server.abort();
        }
    }
}

#[tokio::test]
async fn configuration_reserves_admission_and_a_takeover_after_dispatch_keeps_the_authoritative_result()
 {
    for version in [AgentVersion::V1, AgentVersion::V2] {
        let command = format!(
            "{}{}",
            env!("CARGO_BIN_EXE_hive-test-agent"),
            if version == AgentVersion::V1 {
                " v1"
            } else {
                ""
            }
        );
        let mut config = config_with_agent("mock", &command, version);
        config
            .agents
            .get_mut("mock")
            .unwrap()
            .env
            .insert("HIVE_MOCK_PAUSE_CONFIG".into(), "1".into());
        let (address, server) = start_gateway(config).await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        timeout(Duration::from_secs(10), Client.v2().on_receive_notification(
            async move |update: v2::UpdateSessionNotification, _: V2ConnectionTo<Agent>| { tx.send(update).map_err(Error::into_internal_error) },
            agent_client_protocol::on_receive_notification!(),
        ).connect_with(HttpClient::new(format!("ws://{address}")).unwrap(), async move |cx| {
            cx.send_request(v2::InitializeRequest::new(ProtocolVersion::V2, v2::Implementation::new("configuration-race", "1"))).block_task().await?;
            let cwd = std::env::current_dir().map_err(Error::into_internal_error)?;
            let created = cx.send_request(v2::NewSessionRequest::new(cwd.clone())).block_task().await?;
            let id = created.session_id;
            let setter = cx.send_request(v2::SetSessionConfigOptionRequest::new(id.clone(), v2::SessionConfigId::new("model"), v2::SessionConfigOptionValue::Id { value: v2::SessionConfigValueId::new("careful") }).meta(created.meta.clone())).block_task();
            let (sent, received) = oneshot::channel();
            cx.spawn(async move { let _ = sent.send(setter.await); Ok(()) })?;
            while let Some(event) = rx.recv().await {
                if matches!(event.update, v2::SessionUpdate::ConfigOptionUpdate(_)) || matches!(event.update, v2::SessionUpdate::Other(ref other) if other.session_update == "_mock_configuring") { break; }
            }
            assert!(cx.send_request(v2::PromptRequest::new(id.clone(), vec!["must not dispatch".into()]).meta(controlled(&created.meta, "during-config"))).block_task().await.is_err());
            assert!(cx.send_request(v2::SetSessionConfigOptionRequest::new(id.clone(), v2::SessionConfigId::new("model"), v2::SessionConfigOptionValue::Id { value: v2::SessionConfigValueId::new("fast") }).meta(created.meta.clone())).block_task().await.is_err());
            let pilot = cx.send_request(v2::ResumeSessionRequest::new(id.clone(), cwd.clone())).block_task().await?;
            received.await.map_err(Error::into_internal_error)??;
            let resumed = cx.send_request(v2::ResumeSessionRequest::new(id, cwd).meta({ let mut meta = v2::Meta::new(); meta.insert("hive".into(), serde_json::json!({"mode":"observe"})); meta })).block_task().await?;
            assert_eq!(serde_json::to_value(resumed.config_options).unwrap()[0]["currentValue"], "careful");
            assert_ne!(pilot.meta.as_ref().unwrap()["hive"]["generation"], created.meta.as_ref().unwrap()["hive"]["generation"]);
            assert_eq!(summaries(address).await["sessions"][0]["usage"]["turns"], 0);
            Ok::<_, Error>(())
        })).await.unwrap().unwrap();
        server.abort();
    }
}

#[tokio::test]
async fn provider_turn_usage_and_missing_usage_are_isolated_and_not_charged_on_retry() {
    for version in [AgentVersion::V1, AgentVersion::V2] {
        let command = if version == AgentVersion::V1 {
            format!("{} v1", env!("CARGO_BIN_EXE_hive-test-agent"))
        } else {
            env!("CARGO_BIN_EXE_hive-test-agent").into()
        };
        let (address, server) = start_gateway(config_with_agent("mock", &command, version)).await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let client = HttpClient::new(format!("ws://{address}")).unwrap();
        timeout(
            Duration::from_secs(15),
            Client
                .v2()
                .on_receive_notification(
                    async move |update: v2::UpdateSessionNotification, _: V2ConnectionTo<Agent>| {
                        tx.send(update).map_err(Error::into_internal_error)
                    },
                    agent_client_protocol::on_receive_notification!(),
                )
                .connect_with(client, async move |cx| {
                    cx.send_request(v2::InitializeRequest::new(
                        ProtocolVersion::V2,
                        v2::Implementation::new("usage-test", "1"),
                    ))
                    .block_task()
                    .await?;
                    let cwd = std::env::current_dir().map_err(Error::into_internal_error)?;
                    let opened = cx
                        .send_request(v2::NewSessionRequest::new(cwd))
                        .block_task()
                        .await?;
                    let id = opened.session_id;
                    let meta = opened.meta;
                    assert!(
                        meta.as_ref().unwrap()["hive"]["usageSnapshot"]["tokens"]["totalTokens"]
                            .is_null()
                    );
                    let prompt = || {
                        v2::PromptRequest::new(id.clone(), vec!["usage-fixture".into()])
                            .meta(controlled(&meta, "billed"))
                    };
                    cx.send_request(prompt()).block_task().await?;
                    let first = timeout(Duration::from_secs(8), async {
                        while let Some(update) = rx.recv().await {
                            if update.session_id == id
                                && let v2::SessionUpdate::Other(other) = update.update
                                && other.session_update == "_hive_usage"
                                && other.fields["turn"]["id"] == "billed"
                            {
                                return other.fields;
                            }
                        }
                        panic!("usage stream ended")
                    })
                    .await
                    .map_err(Error::into_internal_error)?;
                    assert_eq!(
                        first["turn"]["tokens"]["inputTokens"], 80,
                        "billed turn event: {first:?}"
                    );
                    assert_eq!(first["turn"]["tokens"]["cachedReadTokens"], 20);
                    assert_eq!(first["session"]["turns"], 1);
                    cx.send_request(prompt()).block_task().await?;
                    assert_eq!(summaries(address).await["sessions"][0]["usage"]["turns"], 1);
                    cx.send_request(
                        v2::PromptRequest::new(id.clone(), vec!["usage-missing".into()])
                            .meta(controlled(&meta, "missing")),
                    )
                    .block_task()
                    .await?;
                    let second = timeout(Duration::from_secs(8), async {
                        while let Some(update) = rx.recv().await {
                            if update.session_id == id
                                && let v2::SessionUpdate::Other(other) = update.update
                                && other.session_update == "_hive_usage"
                                && other.fields["turn"]["id"] == "missing"
                            {
                                return other.fields;
                            }
                        }
                        panic!("usage stream ended")
                    })
                    .await
                    .map_err(Error::into_internal_error)?;
                    assert!(second["turn"]["tokens"].is_null());
                    assert_eq!(second["session"]["reportedTurns"], 1);
                    assert_eq!(second["session"]["turns"], 2);
                    assert!(second["session"]["tokens"]["totalTokens"].is_null());
                    Ok::<_, Error>(())
                }),
        )
        .await
        .unwrap()
        .unwrap();
        server.abort();
    }
}

async fn next_revision(stream: &mut reqwest::Response, pending: &mut String) -> u64 {
    timeout(Duration::from_secs(10), async {
        loop {
            if let Some(end) = pending.find("\n\n") {
                let event = pending.drain(..end + 2).collect::<String>();
                if let Some(data) = event.lines().find_map(|line| line.strip_prefix("data: ")) {
                    return data.parse().unwrap();
                }
            }
            let chunk = stream
                .chunk()
                .await
                .unwrap()
                .expect("catalog stream closed");
            pending.push_str(
                &String::from_utf8(chunk.to_vec())
                    .unwrap()
                    .replace("\r\n", "\n"),
            );
        }
    })
    .await
    .unwrap()
}

async fn exercise_gateway(transport: &str, version: AgentVersion) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address: SocketAddr = listener.local_addr().unwrap();
    let command = if version == AgentVersion::V1 {
        format!("{} v1", env!("CARGO_BIN_EXE_hive-test-agent"))
    } else {
        env!("CARGO_BIN_EXE_hive-test-agent").to_string()
    };
    let broker = hive_daemon::Broker::new(config_with_agent("mock", &command, version));
    let origin = format!("http://{address}");
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            hive_daemon::router_with_origin(broker, Some(&origin)),
        )
        .await
        .unwrap();
    });
    let url = format!("{transport}://{address}");
    let status: serde_json::Value = reqwest::get(format!("http://{address}/v1/status"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["sessions"], 0);
    assert_eq!(
        summaries(address).await["sessions"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    let mut events = reqwest::get(format!("http://{address}/v1/sessions/events"))
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let mut pending = String::new();
    let initial = next_revision(&mut events, &mut pending).await;
    let health: serde_json::Value = reqwest::get(format!("http://{address}/v1/health"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(health["status"], "ok");
    let ready: serde_json::Value = reqwest::get(format!("http://{address}/v1/ready"))
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(ready["ready"], true);
    let web = reqwest::get(format!("http://{address}/ui?session=hive-tui"))
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(web.contains("/ui/app.js"));
    assert!(web.contains("/ui/style.css"));
    assert!(!web.contains("/src/main.js"));
    assert!(web.contains("rel=\"icon\""));
    let icon = reqwest::get(format!("http://{address}/ui/icon.svg"))
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert_eq!(icon.headers()["content-type"], "image/svg+xml");
    assert_eq!(
        icon.text().await.unwrap(),
        include_str!("../../../docs/images/icon.svg")
    );
    let script = reqwest::get(format!("http://{address}/ui/app.js"))
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert_eq!(script.headers()["content-type"], "text/javascript");
    assert_eq!(script.headers()["cache-control"], "no-cache");
    assert_eq!(
        script.text().await.unwrap(),
        include_str!("../../../clients/hive-web/dist/app.js")
    );
    let stylesheet = reqwest::get(format!("http://{address}/ui/style.css"))
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert_eq!(stylesheet.headers()["content-type"], "text/css");
    assert_eq!(stylesheet.headers()["cache-control"], "no-cache");
    assert_eq!(
        stylesheet.text().await.unwrap(),
        include_str!("../../../clients/hive-web/dist/style.css")
    );
    // Removed paths must not survive as aliases that keep clients on the old prefix.
    for path in ["/hive/v1/web", "/hive/v1/web/app.js", "/hive/v1/status"] {
        let response = reqwest::get(format!("http://{address}{path}"))
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND, "{path}");
    }
    let provider: serde_json::Value = reqwest::get(format!("http://{address}/v1/providers"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(provider["agents"][0]["id"], "mock");
    assert_eq!(provider["agents"][0]["transport"], "stdio");
    assert!(provider.get("command").is_none());
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut headers = reqwest::header::HeaderMap::new();
    if transport == "ws" {
        headers.insert(
            reqwest::header::ORIGIN,
            format!("http://{address}").parse().unwrap(),
        );
    }
    let client_transport = HttpClient::builder(&url)
        .configure_http(|http| http.default_headers(headers))
        .build()
        .unwrap();
    let flow = Client
        .v2()
        .on_receive_notification(
            async move |update: v2::UpdateSessionNotification, _cx: V2ConnectionTo<Agent>| {
                tx.send(update).map_err(Error::into_internal_error)
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(client_transport, async move |cx| {
            let init = cx
                .send_request(v2::InitializeRequest::new(
                    ProtocolVersion::V2,
                    v2::Implementation::new("gateway-test", "0.1.0"),
                ))
                .block_task()
                .await?;
            assert_eq!(init.protocol_version, ProtocolVersion::V2);
            assert!(init.capabilities.session.is_some());
            let cwd = std::env::current_dir().map_err(Error::into_internal_error)?;
            let opened = cx
                .send_request(v2::NewSessionRequest::new(cwd.clone()))
                .block_task()
                .await?;
            let id = opened.session_id;
            let mut opened_meta = opened.meta;
            let second = cx
                .send_request(v2::NewSessionRequest::new(cwd))
                .block_task()
                .await?;
            let other_id = second.session_id;
            let second_meta = second.meta;
            assert_ne!(id, other_id);
            let created = summaries(address).await;
            assert_eq!(created["sessions"].as_array().unwrap().len(), 2);
            assert!(created["revision"].as_u64().unwrap() > initial);
            for summary in created["sessions"].as_array().unwrap() {
                assert_eq!(summary["agent"], "mock");
                assert!(matches!(
                    summary["activity"].as_str(),
                    Some("unknown" | "idle")
                ));
                assert_eq!(summary["lifecycle"], "active");
                assert!(summary["title"].is_null());
                if summary["activity"] == "unknown" {
                    assert!(summary["lastActivityAt"].is_null());
                } else {
                    assert!(summary["lastActivityAt"].as_u64().is_some());
                }
                assert!(summary["controlOwner"].as_str().is_some());
                assert!(summary.get("token").is_none());
            }
            let event_revision = next_revision(&mut events, &mut pending).await;
            assert!(event_revision > initial);
            // Publish both requests before waiting for either completion.
            let first_text = if version == AgentVersion::V1 {
                "activity-sequence hello hive"
            } else {
                "hello hive"
            };
            let first_prompt = cx
                .send_request(
                    v2::PromptRequest::new(id.clone(), vec![first_text.into()])
                        .meta(controlled(&opened_meta, "first")),
                )
                .block_task();
            let second_prompt = cx
                .send_request(
                    v2::PromptRequest::new(other_id.clone(), vec!["other session".into()])
                        .meta(controlled(&second_meta, "second")),
                )
                .block_task();
            let (response, other_response) = tokio::try_join!(first_prompt, second_prompt)?;
            assert!(!response.message_id.to_string().is_empty());
            assert!(!other_response.message_id.to_string().is_empty());
            let mut texts = [false; 2];
            let mut running = [false; 2];
            let mut idle = [false; 2];
            let mut saw_v1_tool = false;
            let mut saw_v1_plan = false;
            while !idle.iter().all(|done| *done) {
                let update = timeout(Duration::from_secs(10), rx.recv())
                    .await
                    .map_err(Error::into_internal_error)?
                    .ok_or_else(Error::internal_error)?;
                let index = if update.session_id == id {
                    0
                } else if update.session_id == other_id {
                    1
                } else {
                    panic!("unrelated session update");
                };
                match update.update {
                    v2::SessionUpdate::AgentMessageChunk(chunk) => {
                        if let v2::ContentBlock::Text(text) = chunk.content {
                            assert_eq!(
                                text.text,
                                if index == 0 {
                                    if version == AgentVersion::V1 { "Echo: activity-sequence hello hive" } else { "Echo: hello hive" }
                                } else {
                                    "Echo: other session"
                                }
                            );
                            texts[index] = true;
                        }
                    }
                    v2::SessionUpdate::StateUpdate(v2::StateUpdate::Running(_)) => {
                        running[index] = true
                    }
                    v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(_)) if running[index] => {
                        idle[index] = true;
                    }
                    v2::SessionUpdate::Other(other) if index == 0 && other.session_update == "_hive_activity" => {
                        if other.fields["kind"] == "tool" && other.fields["tool"]["status"] == "failed" { saw_v1_tool = true; }
                        if other.fields["kind"] == "plan" && other.fields["plan"]["entries"][0]["status"] == "in_progress" { saw_v1_plan = true; }
                    }
                    _ => {}
                }
            }
            assert!(texts.iter().all(|seen| *seen));
            if version == AgentVersion::V1 {
                assert!(saw_v1_tool && saw_v1_plan, "v1 tool and plan activity must stream before idle");
            }
            let listed = cx
                .send_request(v2::ListSessionsRequest::new())
                .block_task()
                .await?;
            assert_eq!(listed.sessions.len(), 2);
            let after = summaries(address).await;
            assert!(after["revision"].as_u64().unwrap() > created["revision"].as_u64().unwrap());
            let listed_summaries = after["sessions"].as_array().unwrap();
            assert_eq!(listed_summaries[0]["activity"], "idle");
            assert_eq!(listed_summaries[1]["activity"], "idle");
            assert!(listed_summaries[0]["lastActivityAt"].as_u64().is_some());
            assert!(listed_summaries[1]["lastActivityAt"].as_u64().is_some());
            for entry in &listed.sessions {
                let summary = listed_summaries
                    .iter()
                    .find(|summary| summary["sessionId"] == entry.session_id.to_string())
                    .unwrap();
                let hive = &entry.meta.as_ref().unwrap()["hive"];
                assert_eq!(hive["title"], summary["title"]);
                assert_eq!(hive["activity"], summary["activity"]);
                assert_eq!(hive["controlOwner"], summary["controlOwner"]);
                assert_eq!(hive["generation"], summary["generation"]);
                assert!(hive.get("token").is_none());
            }
            let cwd = std::env::current_dir().map_err(Error::into_internal_error)?;
            let filtered = cx
                .send_request(v2::ListSessionsRequest::new().cwd(cwd))
                .block_task()
                .await?;
            assert_eq!(filtered.sessions.len(), 2);
            let elsewhere = std::env::current_dir()
                .map_err(Error::into_internal_error)?
                .join("elsewhere");
            let unmatched = cx
                .send_request(v2::ListSessionsRequest::new().cwd(elsewhere))
                .block_task()
                .await?;
            assert!(unmatched.sessions.is_empty());
            if version == AgentVersion::V2 {
                let cwd = std::env::current_dir().map_err(Error::into_internal_error)?;
                let request = v2::ResumeSessionRequest::new(id.clone(), cwd)
                    .replay_from(v2::ReplayFrom::Start(v2::ReplayFromStart::new()));
                let resumed = cx.send_request(request).block_task().await?;
                opened_meta = resumed.meta;
                let mut replayed = false;
                while let Ok(update) = rx.try_recv() {
                    if update.session_id == id
                        && matches!(update.update, v2::SessionUpdate::AgentMessage(_))
                    {
                        replayed = true;
                    }
                }
                assert!(replayed, "resume must replay retained snapshots");
                cx.send_notification(
                    v2::CancelSessionNotification::new(id.clone())
                        .meta(controlled(&opened_meta, "cancel")),
                )?;
                let cancelled = timeout(Duration::from_secs(10), async {
                    while let Some(update) = rx.recv().await {
                        if update.session_id == id
                            && matches!(
                                update.update,
                                v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(_))
                            )
                        {
                            return true;
                        }
                    }
                    false
                })
                .await
                .map_err(Error::into_internal_error)?;
                assert!(cancelled);
            }
            cx.send_request(v2::CloseSessionRequest::new(id.clone()).meta(opened_meta.unwrap()))
                .block_task()
                .await?;
            cx.send_request(
                v2::PromptRequest::new(other_id.clone(), vec!["still running".into()])
                    .meta(controlled(&second_meta, "third")),
            )
            .block_task()
            .await?;
            let continued = timeout(Duration::from_secs(10), async {
                let mut saw_reply = false;
                while let Some(update) = rx.recv().await {
                    if update.session_id == other_id {
                        match update.update {
                            v2::SessionUpdate::AgentMessageChunk(chunk)
                                if matches!(&chunk.content, v2::ContentBlock::Text(text) if text.text == "Echo: still running") =>
                            {
                                saw_reply = true;
                            }
                            v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(_)) if saw_reply => {
                                // A chunk precedes the agent's idle transition; closing before idle races the worker.
                                return true;
                            }
                            _ => {}
                        }
                    }
                }
                false
            })
            .await
            .map_err(Error::into_internal_error)?;
            assert!(
                continued,
                "closing one session must leave the other running"
            );
            cx.send_request(v2::CloseSessionRequest::new(other_id).meta(second_meta.unwrap()))
                .block_task()
                .await?;
            assert!(
                summaries(address).await["sessions"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
            let mut reconnected = reqwest::get(format!("http://{address}/v1/sessions/events"))
                .await
                .unwrap()
                .error_for_status()
                .unwrap();
            let latest = next_revision(&mut reconnected, &mut String::new()).await;
            assert_eq!(
                latest,
                summaries(address).await["revision"].as_u64().unwrap()
            );
            Ok(())
        });
    timeout(Duration::from_secs(30), flow)
        .await
        .unwrap()
        .unwrap();
    server.abort();
}

#[tokio::test]
async fn a_remote_listener_is_rejected_without_authentication() {
    let error = hive_daemon::serve("0.0.0.0:0".parse().unwrap(), Config::default())
        .await
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
}

fn config_with_agent(id: &str, command: &str, version: AgentVersion) -> Config {
    let mut agents = BTreeMap::new();
    let mut words = command.split_whitespace();
    agents.insert(
        id.to_string(),
        Profile {
            name: None,
            command: Some(words.next().unwrap().to_string()),
            args: words.map(str::to_string).collect(),
            env: BTreeMap::new(),
            endpoint: None,
            headers: BTreeMap::new(),
            versions: vec![version],
            pricing: Default::default(),
            kind: Default::default(),
            username: None,
            password_env: None,
        },
    );
    Config {
        default_agent: Some(id.to_string()),
        agents,
    }
}

async fn start_gateway(config: Config) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let origin = format!("http://{address}");
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            hive_daemon::router_with_origin(hive_daemon::Broker::new(config), Some(&origin)),
        )
        .await
        .unwrap();
    });
    (address, server)
}

/// Bypass the client's typed negotiation guard to exercise the gateway's wire boundary.
async fn raw_initialize(url: &str, params: serde_json::Value) -> serde_json::Value {
    use agent_client_protocol::ConnectTo;

    let (mut channel, driver) =
        ConnectTo::<Client>::into_channel_and_future(HttpClient::new(url).unwrap());
    let driver = tokio::spawn(driver.unwrap());
    let request = serde_json::json!({
        "jsonrpc": "2.0", "id": "negotiation", "method": "initialize", "params": params
    });
    let response = raw_exchange(&mut channel, request).await;
    driver.abort();
    response
}

async fn raw_exchange(
    channel: &mut agent_client_protocol::Channel,
    request: serde_json::Value,
) -> serde_json::Value {
    use agent_client_protocol::TransportFrame;
    use tokio_stream::StreamExt;

    channel
        .tx
        .unbounded_send(TransportFrame::parse_json(&request.to_string()))
        .unwrap();
    let response = timeout(Duration::from_secs(5), channel.rx.next())
        .await
        .unwrap()
        .unwrap();
    serde_json::from_str(&response.to_json().unwrap()).unwrap()
}

#[tokio::test]
async fn failed_websocket_negotiation_cannot_create_a_session_and_can_be_retried() {
    use agent_client_protocol::ConnectTo;

    let config = config_with_agent(
        "mock",
        env!("CARGO_BIN_EXE_hive-test-agent"),
        AgentVersion::V2,
    );
    let (address, server) = start_gateway(config).await;
    let (mut channel, driver) = ConnectTo::<Client>::into_channel_and_future(
        HttpClient::new(format!("ws://{address}")).unwrap(),
    );
    let driver = tokio::spawn(driver.unwrap());
    let mut initialize = serde_json::json!({"jsonrpc":"2.0", "id":1, "method":"initialize", "params":{
        "protocolVersion":1, "info":{"name":"retry-test","version":"1"}, "capabilities":{}
    }});
    assert_eq!(
        raw_exchange(&mut channel, initialize.clone()).await["error"]["code"],
        -32022
    );
    let blocked = raw_exchange(&mut channel, serde_json::json!({
        "jsonrpc":"2.0", "id":2, "method":"session/new", "params":{"cwd":std::env::current_dir().unwrap()}
    })).await;
    assert!(blocked.get("error").is_some(), "{blocked}");
    assert!(
        summaries(address).await["sessions"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    initialize["params"]["protocolVersion"] = 2.into();
    assert_eq!(
        raw_exchange(&mut channel, initialize).await["result"]["protocolVersion"],
        2
    );
    driver.abort();
    server.abort();
}

#[tokio::test]
async fn gateway_negotiation_errors_and_future_versions_match_the_draft_over_both_transports() {
    let (address, server) = start_gateway(Config::default()).await;
    for url in [format!("ws://{address}"), format!("http://{address}")] {
        let params = |version| {
            serde_json::json!({
                "protocolVersion": version, "info": {"name": "wire-test", "version": "1"},
                "capabilities": {}
            })
        };
        let rejected = raw_initialize(&url, params(serde_json::json!(1))).await;
        assert_eq!(rejected["id"], "negotiation");
        assert_eq!(rejected["error"]["code"], -32022);
        assert_eq!(
            rejected["error"]["data"],
            serde_json::json!({"requested": 1, "supported": [2]})
        );
        for version in [
            serde_json::Value::Null,
            serde_json::json!("2"),
            serde_json::json!(2.5),
            serde_json::json!(-1),
            serde_json::json!(65536),
        ] {
            let rejected = raw_initialize(&url, params(version)).await;
            assert_eq!(rejected["id"], "negotiation");
            assert_eq!(rejected["error"]["code"], -32602, "{rejected}");
        }
        let mut missing = params(serde_json::json!(2));
        missing.as_object_mut().unwrap().remove("protocolVersion");
        assert_eq!(raw_initialize(&url, missing).await["error"]["code"], -32602);
        for version in [2, 3] {
            let accepted = raw_initialize(&url, params(serde_json::json!(version))).await;
            assert_eq!(accepted["result"]["protocolVersion"], 2, "{accepted}");
            assert_eq!(
                accepted["result"]["capabilities"]["session"],
                serde_json::json!({})
            );
        }
    }
    assert!(
        summaries(address).await["sessions"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    server.abort();
}

#[test]
fn restored_blocked_sessions_reject_new_work_on_both_transports() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let base = runtime.block_on(restore_conversation(AgentVersion::V2, "blocked"));
    // Release connection watchers before removing SQLite fixtures on Windows.
    drop(runtime);
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
fn earlier_conversations_restore_once_under_the_original_identity_for_both_acp_versions() {
    for (version, mode) in [
        (AgentVersion::V1, "both"),
        (AgentVersion::V1, "load-only"),
        (AgentVersion::V1, "resume-only"),
        (AgentVersion::V1, "none"),
        (AgentVersion::V1, "empty"),
        (AgentVersion::V1, "live"),
        (AgentVersion::V2, "both"),
        (AgentVersion::V2, "reject"),
        (AgentVersion::V2, "missing"),
        (AgentVersion::V2, "empty"),
        (AgentVersion::V2, "live"),
        (AgentVersion::V2, "partial"),
        (AgentVersion::V2, "no-state"),
        (AgentVersion::V2, "unimplemented"),
    ] {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let base = runtime.block_on(restore_conversation(version, mode));
        // Connection watchers can retain the database after broker shutdown. Drop their
        // runtime before unlinking fixtures so Windows sees no outstanding SQLite handles.
        drop(runtime);
        std::fs::remove_dir_all(base).unwrap();
    }
}

async fn transcript_snapshot(
    address: SocketAddr,
    id: v2::SessionId,
) -> agent_client_protocol::Result<(Vec<String>, u64)> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let boundary = tx.clone();
    Client
        .v2()
        .on_receive_notification(
            async move |update: v2::UpdateSessionNotification, _: V2ConnectionTo<Agent>| {
                tx.send(Some(update)).map_err(Error::into_internal_error)
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(
            HttpClient::new(format!("ws://{address}")).unwrap(),
            async move |cx| {
                cx.send_request(v2::InitializeRequest::new(
                    ProtocolVersion::V2,
                    v2::Implementation::new("reload", "1"),
                ))
                .block_task()
                .await?;
                let mut meta = v2::Meta::new();
                meta.insert("hive".into(), serde_json::json!({"mode":"observe"}));
                let (completed, response) = oneshot::channel();
                cx.prepare_request(
                    v2::ResumeSessionRequest::new(id.clone(), std::env::current_dir().unwrap())
                        .replay_from(v2::ReplayFrom::Start(v2::ReplayFromStart::new()))
                        .meta(meta),
                )
                .on_receiving_result(async move |result| {
                    // An ordered marker closes the entire replay batch, including duplicates.
                    let _ = boundary.send(None);
                    let _ = completed.send(result);
                    Ok(())
                })?;
                let resumed = response.await.map_err(|_| Error::internal_error())??;
                let hive = &resumed.meta.unwrap()["hive"];
                assert_eq!(hive["role"], "observer");
                let through = hive["snapshotThroughSeq"].as_u64().unwrap();
                let mut texts = Vec::new();
                while let Some(event) = timeout(Duration::from_secs(5), rx.recv())
                    .await
                    .unwrap()
                    .unwrap()
                {
                    assert_eq!(event.session_id, id);
                    if matches!(event.update, v2::SessionUpdate::StateUpdate(_)) {
                        assert_ne!(event.meta.as_ref().unwrap()["hive"]["snapshot"], true);
                        assert!(
                            event.meta.as_ref().unwrap()["hive"]["seq"]
                                .as_u64()
                                .unwrap()
                                > through
                        );
                        continue;
                    }
                    assert_eq!(event.meta.as_ref().unwrap()["hive"]["snapshot"], true);
                    let value = serde_json::to_value(event.update).unwrap();
                    assert!(matches!(
                        value["sessionUpdate"].as_str(),
                        Some("user_message" | "agent_message")
                    ));
                    texts.push(value["content"][0]["text"].as_str().unwrap().to_owned());
                }
                Ok((texts, through))
            },
        )
        .await
}

async fn post_recovery_updates_and_permissions_are_live(address: SocketAddr, id: v2::SessionId) {
    let (tx, mut rx) = mpsc::unbounded_channel();
    Client
        .v2()
        .on_receive_request(
            async move |_request: v2::RequestPermissionRequest,
                        responder: Responder<v2::RequestPermissionResponse>,
                        _: V2ConnectionTo<Agent>| {
                tx.send(()).map_err(Error::into_internal_error)?;
                responder.respond(v2::RequestPermissionResponse::new(
                    v2::RequestPermissionOutcome::Cancelled,
                ))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(
            HttpClient::new(format!("ws://{address}")).unwrap(),
            async move |cx| {
                cx.send_request(v2::InitializeRequest::new(
                    ProtocolVersion::V2,
                    v2::Implementation::new("post-recovery", "1"),
                ))
                .block_task()
                .await?;
                let resume =
                    || v2::ResumeSessionRequest::new(id.clone(), std::env::current_dir().unwrap());
                let mut observe = v2::Meta::new();
                observe.insert("hive".into(), serde_json::json!({"mode":"observe"}));
                let attached = cx.send_request(resume().meta(observe)).block_task().await?;
                let mut takeover = resume();
                if attached.meta.as_ref().unwrap()["hive"]["recoveryRequired"] == true {
                    // Fast v2 idle can precede durable acknowledgement; this fixture has inspected
                    // both completed turns and must explicitly acknowledge before taking control.
                    let mut acknowledge = v2::Meta::new();
                    acknowledge.insert(
                        "hive".into(),
                        serde_json::json!({"recoveryAction":"acknowledge"}),
                    );
                    takeover = takeover.meta(acknowledge);
                }
                cx.send_request(takeover).block_task().await?;
                // Permission is sent after config/usage on the downstream connection; observing it
                // proves those immediately post-response messages have crossed dispatch as live work.
                timeout(Duration::from_secs(5), rx.recv())
                    .await
                    .unwrap()
                    .unwrap();
                let current = cx.send_request(resume()).block_task().await?;
                assert_eq!(
                    serde_json::to_value(current.config_options).unwrap()[0]["currentValue"],
                    "careful"
                );
                assert_eq!(
                    current.meta.unwrap()["hive"]["usageSnapshot"]["contextUsed"],
                    321
                );
                Ok(())
            },
        )
        .await
        .unwrap();
}

async fn restore_conversation(version: AgentVersion, mode: &str) -> std::path::PathBuf {
    let base = std::env::temp_dir().join(format!("hive-transcript-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&base).unwrap();
    let database = base.join("hive/sessions.sqlite3");
    let agent_state = base.join("agent.json");
    let command = format!(
        "{}{}",
        env!("CARGO_BIN_EXE_hive-test-agent"),
        if version == AgentVersion::V1 {
            " v1"
        } else {
            ""
        }
    );
    let mut config = config_with_agent("mock", &command, version);
    let profile = config.agents.get_mut("mock").unwrap();
    profile
        .env
        .insert("HIVE_MOCK_STATE".into(), agent_state.display().to_string());
    profile.env.insert("HIVE_MOCK_RESTORE".into(), mode.into());
    profile.env.insert("HIVE_MOCK_ACK_META".into(), "1".into());
    if std::env::var_os("LLVM_PROFILE_FILE").is_some() {
        profile.env.insert(
            "LLVM_PROFILE_FILE".into(),
            if cfg!(windows) { "NUL" } else { "/dev/null" }.into(),
        );
    }
    let broker = hive_daemon::Broker::persistent(config.clone(), database.clone()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = hive_daemon::router(broker.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (id, acknowledgements) = Client
        .v2()
        .on_receive_notification(
            async move |update: v2::UpdateSessionNotification, _: V2ConnectionTo<Agent>| {
                tx.send(update).map_err(Error::into_internal_error)
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(
            HttpClient::new(format!("ws://{address}")).unwrap(),
            async move |cx| {
                cx.send_request(v2::InitializeRequest::new(
                    ProtocolVersion::V2,
                    v2::Implementation::new("original", "1"),
                ))
                .block_task()
                .await?;
                let opened = cx
                    .send_request(v2::NewSessionRequest::new(std::env::current_dir().unwrap()))
                    .block_task()
                    .await?;
                let mut acknowledgements = Vec::new();
                for text in ["first", "second"] {
                    let acknowledgement = cx
                        .send_request(
                            v2::PromptRequest::new(opened.session_id.clone(), vec![text.into()])
                                .meta(controlled(&opened.meta, text)),
                        )
                        .block_task()
                        .await?;
                    let retried = cx
                        .send_request(
                            v2::PromptRequest::new(opened.session_id.clone(), vec![text.into()])
                                .meta(controlled(&opened.meta, text)),
                        )
                        .block_task()
                        .await?;
                    assert_eq!(retried.message_id, acknowledgement.message_id);
                    assert_eq!(retried.meta, acknowledgement.meta);
                    acknowledgements.push(acknowledgement.message_id);
                    loop {
                        let update = timeout(Duration::from_secs(5), rx.recv())
                            .await
                            .unwrap()
                            .unwrap();
                        if matches!(
                            update.update,
                            v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(_))
                        ) {
                            break;
                        }
                    }
                }
                Ok((opened.session_id, acknowledgements))
            },
        )
        .await
        .unwrap();
    let expected = vec!["first", "Echo: first", "second", "Echo: second"];
    let (texts, old_seq) = transcript_snapshot(address, id.clone()).await.unwrap();
    assert_eq!(texts, expected);
    let fresh = summaries(address).await;
    assert_eq!(fresh["sessions"][0]["transcript"]["owner"], "agent");
    assert_eq!(fresh["sessions"][0]["transcript"]["source"], "live");
    assert_eq!(
        fresh["sessions"][0]["transcript"]["availability"],
        "available"
    );
    if version == AgentVersion::V1 && matches!(mode, "resume-only" | "none") {
        // Unsupported future restoration does not invalidate content observed from creation.
        assert_eq!(
            fresh["sessions"][0]["transcript"]["restoration"],
            "unsupported"
        );
    }
    assert_eq!(
        transcript_snapshot(address, id.clone()).await.unwrap().0,
        expected
    );
    let before: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&agent_state).unwrap()).unwrap();
    broker.shutdown().await;
    server.abort();
    let _ = server.await;
    drop(broker);

    let recovered = hive_daemon::Broker::persistent(config, database.clone()).unwrap();
    recovered.reconcile().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = hive_daemon::router(recovered.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let catalog = summaries(address).await;
    assert_eq!(catalog["sessions"][0]["sessionId"], id.to_string());
    if mode == "blocked" {
        assert_eq!(catalog["sessions"][0]["activity"], "requires_action");
        for scheme in ["ws", "http"] {
            Client
                .v2()
                .connect_with(
                    HttpClient::new(format!("{scheme}://{address}")).unwrap(),
                    async |cx| {
                        cx.send_request(v2::InitializeRequest::new(
                            ProtocolVersion::V2,
                            v2::Implementation::new("restored-blocked-test", "1"),
                        ))
                        .block_task()
                        .await?;
                        let mut request = v2::ResumeSessionRequest::new(
                            id.clone(),
                            std::env::current_dir().unwrap(),
                        );
                        // A preceding transport may already have acknowledged recovery.
                        if summaries(address).await["sessions"][0]["recovery"]
                            == "recovery_required"
                        {
                            request = request.meta(v2::Meta::from_iter([(
                                "hive".into(),
                                serde_json::json!({"recoveryAction":"acknowledge"}),
                            )]));
                        }
                        let attached = cx.send_request(request).block_task().await?;
                        assert_blocked_admission(&cx, &id, &attached.meta).await
                    },
                )
                .await
                .unwrap();
        }
    }
    if mode == "both" {
        Client
            .v2()
            .connect_with(
                HttpClient::new(format!("ws://{address}")).unwrap(),
                async |cx| {
                    cx.send_request(v2::InitializeRequest::new(
                        ProtocolVersion::V2,
                        v2::Implementation::new("retry-test", "1"),
                    ))
                    .block_task()
                    .await?;
                    let mut request =
                        v2::ResumeSessionRequest::new(id.clone(), std::env::current_dir().unwrap());
                    if catalog["sessions"][0]["recovery"] == "recovery_required" {
                        request = request.meta(v2::Meta::from_iter([(
                            "hive".into(),
                            serde_json::json!({"recoveryAction":"acknowledge"}),
                        )]));
                    }
                    let attached = cx.send_request(request).block_task().await?;
                    for (text, message_id) in ["first", "second"].into_iter().zip(&acknowledgements)
                    {
                        let retried = cx
                            .send_request(
                                v2::PromptRequest::new(id.clone(), vec![text.into()])
                                    .meta(controlled(&attached.meta, text)),
                            )
                            .block_task()
                            .await?;
                        assert_eq!(&retried.message_id, message_id);
                        assert!(
                            retried.meta.is_none(),
                            "durable acknowledgements exclude arbitrary metadata"
                        );
                        let changed = cx
                            .send_request(
                                v2::PromptRequest::new(id.clone(), vec!["different".into()])
                                    .meta(controlled(&attached.meta, text)),
                            )
                            .block_task()
                            .await
                            .unwrap_err();
                        assert!(
                            changed
                                .data
                                .unwrap()
                                .to_string()
                                .contains("different content")
                        );
                    }
                    Ok(())
                },
            )
            .await
            .unwrap();
    }
    if mode == "live" {
        post_recovery_updates_and_permissions_are_live(address, id.clone()).await;
    }
    match mode {
        "none" | "reject" | "missing" | "unimplemented" => {
            assert_eq!(
                catalog["sessions"][0]["transcript"]["availability"],
                "unavailable"
            );
            let reason = catalog["sessions"][0]["recoveryReason"]
                .as_str()
                .unwrap_or_else(|| catalog["sessions"][0]["reason"].as_str().unwrap_or(""));
            assert!(
                reason.contains(if mode == "none" {
                    "hive::transcript::unsupported"
                } else if mode == "missing" {
                    "hive::transcript::session_missing"
                } else if mode == "unimplemented" {
                    "hive::transcript::operation_rejected"
                } else {
                    "hive::transcript::replay_failed"
                }),
                "{catalog}"
            );
        }
        "resume-only" => {
            let error = transcript_snapshot(address, id.clone()).await.unwrap_err();
            assert!(
                error
                    .data
                    .unwrap()
                    .to_string()
                    .contains("hive::transcript::unsupported")
            );
            assert_eq!(
                catalog["sessions"][0]["transcript"]["restoration"],
                "unsupported"
            );
        }
        "empty" => {
            assert!(
                transcript_snapshot(address, id.clone())
                    .await
                    .unwrap()
                    .0
                    .is_empty()
            );
            let status = &catalog["sessions"][0]["transcript"];
            assert_eq!(status["availability"], "available");
            assert_eq!(status["completeness"], "unknown");
            assert_eq!(
                status["warnings"][0]["code"],
                "hive::transcript::history_missing"
            );
        }
        "partial" => {
            assert_eq!(
                transcript_snapshot(address, id.clone()).await.unwrap().0,
                ["second", "Echo: second"]
            );
            // The protocol exposes no retention watermark: a shorter replay is not proof of violation.
            assert_eq!(
                catalog["sessions"][0]["transcript"]["completeness"],
                "unknown"
            );
        }
        _ => {
            let (texts, seq) = transcript_snapshot(address, id.clone()).await.unwrap();
            assert_eq!(texts, expected, "{version:?} {mode}");
            assert!(seq > old_seq, "restart must not reuse old live cursors");
            assert_eq!(transcript_snapshot(address, id).await.unwrap().0, expected);
            let status = &catalog["sessions"][0]["transcript"];
            assert_eq!(status["source"], "agent_history");
            assert_eq!(status["restoration"], "supported");
            assert_eq!(status["completeness"], "unknown");
            if mode == "no-state" {
                assert_eq!(
                    status["warnings"][0]["code"],
                    "hive::transcript::state_missing"
                );
            }
        }
    }
    let after: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&agent_state).unwrap()).unwrap();
    assert_eq!(
        before["next"], after["next"],
        "restoration cannot create a replacement"
    );
    assert_eq!(
        before["sessions"], after["sessions"],
        "restoration cannot execute another v2 prompt"
    );
    assert_eq!(
        before["v1_sessions"], after["v1_sessions"],
        "restoration cannot execute another v1 prompt"
    );
    if matches!(mode, "both" | "load-only" | "resume-only") {
        assert_eq!(
            after["restores"],
            serde_json::json!([if version == AgentVersion::V2 {
                "v2:start"
            } else if mode == "resume-only" {
                "v1:resume"
            } else {
                "v1:load"
            }])
        );
    }
    recovered.shutdown().await;
    server.abort();
    let _ = server.await;
    drop(recovered);
    if mode == "live" {
        let db = rusqlite::Connection::open(database).unwrap();
        let saved: String = db
            .query_row("SELECT record FROM sessions", [], |row| row.get(0))
            .unwrap();
        let saved: serde_json::Value = serde_json::from_str(&saved).unwrap();
        assert_eq!(
            saved["usage"]["contextUsed"], 321,
            "recovery publication must not overwrite live accounting"
        );
    }
    // History policy must not remove the separate journal that prevents duplicate execution.
    let db = rusqlite::Connection::open(base.join("hive/sessions.sqlite3")).unwrap();
    let prompts: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM prompts WHERE result IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(prompts, 2);
    let mut statement = db
        .prepare("SELECT content,content_fingerprint,result FROM prompts")
        .unwrap();
    for row in statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .unwrap()
    {
        let (content, fingerprint, result) = row.unwrap();
        assert!(content.is_empty());
        assert!(fingerprint.starts_with("sha256-v1:"));
        assert!(!result.contains("sentinel"));
        assert!(!result.contains("_meta"));
    }
    drop(statement);
    drop(db);
    base
}

#[tokio::test]
async fn a_restart_reattaches_the_original_downstream_session_without_creating_another() {
    let base = std::env::temp_dir().join(format!("hive-recovery-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&base).unwrap();
    let database = base.join("hive/sessions.sqlite3");
    let agent_state = base.join("agent.json");
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
        .insert("HIVE_MOCK_STATE".into(), agent_state.display().to_string());
    if std::env::var_os("LLVM_PROFILE_FILE").is_some() {
        // Abruptly stopped test agents cannot safely flush coverage profiles;
        // only the parent test's profile should be collected by llvm-cov.
        config.agents.get_mut("mock").unwrap().env.insert(
            "LLVM_PROFILE_FILE".into(),
            if cfg!(windows) { "NUL" } else { "/dev/null" }.into(),
        );
    }
    let broker = hive_daemon::Broker::persistent(config.clone(), database.clone()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = hive_daemon::router(broker.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let id = Client
        .v2()
        .connect_with(
            HttpClient::new(format!("ws://{address}")).unwrap(),
            async |cx| {
                cx.send_request(v2::InitializeRequest::new(
                    ProtocolVersion::V2,
                    v2::Implementation::new("recovery-test", "1"),
                ))
                .block_task()
                .await?;
                let cwd = std::env::current_dir().unwrap();
                let opened = cx
                    .send_request(v2::NewSessionRequest::new(cwd))
                    .block_task()
                    .await?;
                cx.send_request(
                    v2::SetSessionConfigOptionRequest::new(
                        opened.session_id.clone(),
                        v2::SessionConfigId::new("model"),
                        v2::SessionConfigOptionValue::Id {
                            value: v2::SessionConfigValueId::new("careful"),
                        },
                    )
                    .meta(opened.meta),
                )
                .block_task()
                .await?;
                Ok(opened.session_id)
            },
        )
        .await
        .unwrap();
    assert_eq!(
        summaries(address).await["sessions"][0]["sessionId"],
        id.to_string()
    );
    broker.shutdown().await;
    server.abort();
    let _ = server.await;
    drop(broker);
    let recovered = hive_daemon::Broker::persistent(config.clone(), database.clone()).unwrap();
    recovered.reconcile().await;
    recovered.reconcile().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = hive_daemon::router(recovered.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let catalog = summaries(address).await;
    assert_eq!(catalog["sessions"][0]["sessionId"], id.to_string());
    assert_eq!(catalog["sessions"][0]["recovery"], "reattached");
    Client
        .v2()
        .connect_with(
            HttpClient::new(format!("ws://{address}")).unwrap(),
            async |cx| {
                cx.send_request(v2::InitializeRequest::new(
                    ProtocolVersion::V2,
                    v2::Implementation::new("recovery-test", "1"),
                ))
                .block_task()
                .await?;
                let cwd = std::env::current_dir().unwrap();
                let attached = cx
                    .send_request(v2::ResumeSessionRequest::new(id.clone(), cwd))
                    .block_task()
                    .await?;
                assert_eq!(
                    serde_json::to_value(attached.config_options).unwrap()[0]["currentValue"],
                    "careful"
                );
                assert_eq!(attached.meta.unwrap()["hive"]["role"], "pilot");
                Ok(())
            },
        )
        .await
        .unwrap();
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&agent_state).unwrap()).unwrap();
    assert_eq!(saved["sessions"].as_object().unwrap().len(), 1);
    recovered.shutdown().await;
    server.abort();
    let _ = server.await;
    drop(recovered);
    let db = rusqlite::Connection::open(&database).unwrap();
    let mut record: serde_json::Value = serde_json::from_str(
        &db.query_row(
            "SELECT record FROM sessions WHERE id=?1",
            [id.to_string()],
            |row| row.get::<_, String>(0),
        )
        .unwrap(),
    )
    .unwrap();
    record["uncertain"] = true.into();
    db.execute(
        "UPDATE sessions SET record=?2 WHERE id=?1",
        rusqlite::params![id.to_string(), record.to_string()],
    )
    .unwrap();
    db.execute(
        "INSERT INTO prompts (session_id,key,content,content_fingerprint) VALUES (?1,'interrupted','','sha256-v1:f8edd0d31cc3b2dc29f713081a548677cae8f62da13aae23dfbe9576680b4738')",
        [id.to_string()],
    )
    .unwrap();
    db.execute(
        "INSERT INTO prompts (session_id,key,content,content_fingerprint,result) VALUES (?1,'failed','','sha256-v1:f8edd0d31cc3b2dc29f713081a548677cae8f62da13aae23dfbe9576680b4738','{\"status\":\"failed\"}')",
        [id.to_string()],
    ).unwrap();
    drop(db);
    let recovered = hive_daemon::Broker::persistent(config.clone(), database.clone()).unwrap();
    recovered.reconcile().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = hive_daemon::router(recovered.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    assert_eq!(
        summaries(address).await["sessions"][0]["recovery"],
        "recovery_required"
    );
    Client
        .v2()
        .connect_with(
            HttpClient::new(format!("ws://{address}")).unwrap(),
            async |cx| {
                cx.send_request(v2::InitializeRequest::new(
                    ProtocolVersion::V2,
                    v2::Implementation::new("recovery-test", "1"),
                ))
                .block_task()
                .await?;
                let cwd = std::env::current_dir().unwrap();
                let mut observe = v2::Meta::new();
                observe.insert("hive".into(), serde_json::json!({"mode":"observe"}));
                let attached = cx
                    .send_request(
                        v2::ResumeSessionRequest::new(id.clone(), cwd.clone()).meta(observe),
                    )
                    .block_task()
                    .await?;
                assert_eq!(attached.meta.unwrap()["hive"]["recoveryRequired"], true);
                assert!(
                    cx.send_request(v2::ResumeSessionRequest::new(id.clone(), cwd.clone()))
                        .block_task()
                        .await
                        .is_err()
                );
                let mut acknowledge = v2::Meta::new();
                acknowledge.insert(
                    "hive".into(),
                    serde_json::json!({"recoveryAction":"acknowledge"}),
                );
                let owner = cx
                    .send_request(v2::ResumeSessionRequest::new(id.clone(), cwd).meta(acknowledge))
                    .block_task()
                    .await?;
                let mut meta = owner.meta.unwrap();
                meta.get_mut("hive").unwrap()["key"] = "interrupted".into();
                let result = cx
                    .send_request(v2::PromptRequest::new(id.clone(), vec![]).meta(meta.clone()))
                    .block_task()
                    .await;
                assert!(
                    result
                        .unwrap_err()
                        .data
                        .unwrap()
                        .to_string()
                        .contains("prompt outcome is uncertain after restart"),
                    "a matching interrupted key must never be resent"
                );
                meta.get_mut("hive").unwrap()["key"] = "failed".into();
                let failed = cx
                    .send_request(v2::PromptRequest::new(id.clone(), vec![]).meta(meta))
                    .block_task()
                    .await
                    .unwrap_err();
                assert!(
                    failed
                        .data
                        .unwrap()
                        .to_string()
                        .contains("prompt previously failed")
                );
                Ok(())
            },
        )
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&std::fs::read(&agent_state).unwrap()).unwrap()
            ["sessions"]
            .as_object()
            .unwrap()
            .len(),
        1
    );
    recovered.shutdown().await;
    server.abort();
    let _ = server.await;
    drop(recovered);
    std::fs::remove_file(&agent_state).unwrap();
    let orphaned = hive_daemon::Broker::persistent(config, database).unwrap();
    orphaned.reconcile().await;
    assert_eq!(orphaned_snapshot(&orphaned).await, "lost");
    orphaned.shutdown().await;
    drop(orphaned);
    #[cfg(not(windows))]
    std::fs::remove_dir_all(base).unwrap();
    #[cfg(windows)]
    {
        // Windows holds child-process and SQLite handles until after the test returns;
        // the runner cleans its unique temporary directory when the job ends.
        let _ = std::fs::remove_dir_all(base);
    }
}

async fn orphaned_snapshot(broker: &hive_daemon::Broker) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = hive_daemon::router(broker.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let outcome = summaries(address).await["sessions"][0]["recovery"]
        .as_str()
        .unwrap()
        .to_owned();
    server.abort();
    let _ = server.await;
    outcome
}

async fn create_and_prompt(
    url: &str,
    agent: Option<&str>,
    text: &str,
    expected: &str,
) -> v2::SessionId {
    let client = HttpClient::new(url).unwrap();
    let agent = agent.map(str::to_string);
    let text = text.to_string();
    let expected = expected.to_string();
    let (tx, mut rx) = mpsc::unbounded_channel();
    timeout(
        Duration::from_secs(15),
        Client
            .v2()
            .on_receive_notification(
                async move |update: v2::UpdateSessionNotification, _: V2ConnectionTo<Agent>| {
                    tx.send(update).map_err(Error::into_internal_error)
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(client, async move |cx| {
                cx.send_request(v2::InitializeRequest::new(
                    ProtocolVersion::V2,
                    v2::Implementation::new("selection-test", "1"),
                ))
                .block_task()
                .await?;
                let mut request = v2::NewSessionRequest::new(
                    std::env::current_dir().map_err(Error::into_internal_error)?,
                );
                if let Some(agent) = agent {
                    let mut meta = v2::Meta::new();
                    meta.insert("hive".into(), serde_json::json!({"agent": agent}));
                    request = request.meta(meta);
                }
                let opened = cx.send_request(request).block_task().await?;
                let id = opened.session_id;
                let prompt = v2::PromptRequest::new(
                    id.clone(),
                    vec![v2::ContentBlock::Text(v2::TextContent::new(text))],
                );
                cx.send_request(prompt.meta(controlled(&opened.meta, "selection-prompt")))
                    .block_task()
                    .await?;
                let mut saw_reply = false;
                while let Some(update) = rx.recv().await {
                    if update.session_id != id {
                        continue;
                    }
                    if let v2::SessionUpdate::AgentMessageChunk(chunk) = &update.update
                        && let v2::ContentBlock::Text(content) = &chunk.content
                    {
                        assert_eq!(content.text, expected);
                        saw_reply = true;
                    }
                    if saw_reply
                        && matches!(
                            update.update,
                            v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(_))
                        )
                    {
                        break;
                    }
                }
                assert!(saw_reply);
                Ok(id)
            }),
    )
    .await
    .unwrap()
    .unwrap()
}

#[tokio::test]
async fn live_tools_and_late_tasks_reach_every_observer_and_survive_live_only_resume() {
    let config = config_with_agent(
        "mock",
        env!("CARGO_BIN_EXE_hive-test-agent"),
        AgentVersion::V2,
    );
    let (address, server) = start_gateway(config).await;
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();
    timeout(Duration::from_secs(15), Client.v2()
        .on_receive_notification(async move |event: v2::UpdateSessionNotification, _: V2ConnectionTo<Agent>| {
            observed_tx.send(event).map_err(Error::into_internal_error)
        }, agent_client_protocol::on_receive_notification!())
        .connect_with(HttpClient::new(format!("ws://{address}")).unwrap(), async move |cx| {
            cx.send_request(v2::InitializeRequest::new(ProtocolVersion::V2, v2::Implementation::new("activity-test", "1"))).block_task().await?;
            let cwd = std::env::current_dir().map_err(Error::into_internal_error)?;
            let opened = cx.send_request(v2::NewSessionRequest::new(cwd.clone())).block_task().await?;
            let id = opened.session_id.clone();
            let (ready, attached) = oneshot::channel();
            let observer = tokio::spawn(async move {
                let (tx, mut rx) = mpsc::unbounded_channel();
                Client.v2().on_receive_notification(async move |event: v2::UpdateSessionNotification, _: V2ConnectionTo<Agent>| {
                    tx.send(event).map_err(Error::into_internal_error)
                }, agent_client_protocol::on_receive_notification!())
                .connect_with(HttpClient::new(format!("http://{address}")).unwrap(), async move |cx| {
                    cx.send_request(v2::InitializeRequest::new(ProtocolVersion::V2, v2::Implementation::new("observer", "1"))).block_task().await?;
                    let mut meta = v2::Meta::new();
                    meta.insert("hive".into(), serde_json::json!({"mode":"observe"}));
                    let response = cx.send_request(v2::ResumeSessionRequest::new(id.clone(), cwd.clone()).meta(meta)).block_task().await?;
                    assert_eq!(response.meta.unwrap()["hive"]["activitySnapshot"]["support"]["tools"], "unknown");
                    let _ = ready.send(());
                    let mut kinds = Vec::new();
                    let mut plans = Vec::new();
                    let mut last = 0;
                    while let Some(event) = rx.recv().await {
                        let seq = event.meta.as_ref().unwrap()["hive"]["seq"].as_u64().unwrap();
                        assert!(seq > last);
                        last = seq;
                        if let v2::SessionUpdate::PlanUpdate(plan) = &event.update {
                            plans.push(serde_json::to_value(plan).unwrap());
                        }
                        if let v2::SessionUpdate::Other(other) = event.update
                            && other.session_update == "_hive_activity" {
                                kinds.push(other.fields["kind"].as_str().unwrap().to_owned());
                                if other.fields["kind"] == "task" { break; }
                            }
                    }
                    assert!(kinds.windows(2).any(|window| window == ["tool", "plan"]));
                    assert_eq!(kinds.last().unwrap(), "task");
                     Ok(plans)
                 }).await.unwrap()
            });
            attached.await.map_err(Error::into_internal_error)?;
            cx.send_request(v2::PromptRequest::new(opened.session_id.clone(), vec!["activity-sequence plan-sequence".into()])
                .meta(controlled(&opened.meta, "activity-prompt"))).block_task().await?;
            let mut plans = Vec::new();
            while let Some(event) = observed_rx.recv().await {
                assert_eq!(event.session_id, opened.session_id);
                if let v2::SessionUpdate::PlanUpdate(plan) = &event.update {
                    plans.push(serde_json::to_value(plan).unwrap());
                }
                if let v2::SessionUpdate::Other(other) = &event.update
                    && other.session_update == "_hive_activity" && other.fields["kind"] == "task" { break; }
            }
            assert_eq!(observer.await.map_err(Error::into_internal_error)?, plans);
            assert_eq!(plans.len(), 6);
            assert_eq!(plans[1]["plan"]["entries"][0]["content"], "Inspect");
            assert_eq!(plans[2]["plan"]["entries"][0]["content"], "Verify");
            assert_eq!(plans[3]["plan"]["entries"][0]["status"], "cancelled");
            assert_eq!(plans[1]["_meta"]["update"], true);
            assert_eq!(plans[1]["plan"]["_meta"]["plan"], true);
            assert_eq!(plans[1]["plan"]["entries"][0]["_meta"]["entry"], "Inspect");
            let mut meta = v2::Meta::new();
            meta.insert("hive".into(), serde_json::json!({"mode":"observe"}));
            let response = cx.send_request(v2::ResumeSessionRequest::new(opened.session_id.clone(), std::env::current_dir().unwrap()).meta(meta)).block_task().await?;
            let metadata = response.meta.unwrap();
            let snapshot = &metadata["hive"]["activitySnapshot"];
            assert_eq!(snapshot["foreground"], "idle");
            assert_eq!(snapshot["tools"]["mock-tool"]["status"], "completed");
            assert_eq!(snapshot["tasks"]["background"]["status"], "completed");
            let current = &metadata["hive"]["planSnapshot"];
            assert_eq!(current["plans"]["steps"], plans[3]);
            assert_eq!(current["plans"]["empty"], plans[4]);
            assert_eq!(current["plans"]["custom"], plans[5]);
            let (full, replayed) = plan_attachment(address, opened.session_id.clone(), Some(v2::ReplayFrom::Start(v2::ReplayFromStart::new())), 4).await;
            assert_eq!(full["plans"], current["plans"]);
            for plan in replayed { assert_eq!(full["plans"][plan["plan"]["planId"].as_str().unwrap()], plan); }
            let (incremental, replayed) = plan_attachment(address, opened.session_id, Some(v2::ReplayFrom::Other(v2::OtherReplayFrom::new("_hive_seq", BTreeMap::from([("seq".into(), serde_json::json!(1))])))), 6).await;
            assert_eq!(incremental["plans"], current["plans"]);
            assert_eq!(replayed, plans);
            Ok(())
        })).await.unwrap().unwrap();
    server.abort();
}

async fn plan_attachment(
    address: SocketAddr,
    id: v2::SessionId,
    replay: Option<v2::ReplayFrom>,
    native_count: usize,
) -> (serde_json::Value, Vec<serde_json::Value>) {
    let (tx, mut rx) = mpsc::unbounded_channel();
    timeout(
        Duration::from_secs(10),
        Client
            .v2()
            .on_receive_notification(
                async move |event: v2::UpdateSessionNotification, _: V2ConnectionTo<Agent>| {
                    tx.send(event).map_err(Error::into_internal_error)
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(
                HttpClient::new(format!("http://{address}")).unwrap(),
                async move |cx| {
                    cx.send_request(v2::InitializeRequest::new(
                        ProtocolVersion::V2,
                        v2::Implementation::new("plan-observer", "1"),
                    ))
                    .block_task()
                    .await?;
                    let mut request =
                        v2::ResumeSessionRequest::new(id.clone(), std::env::current_dir().unwrap())
                            .meta(v2::Meta::from_iter([(
                                "hive".into(),
                                serde_json::json!({"mode":"observe"}),
                            )]));
                    let full_snapshot = matches!(replay, Some(v2::ReplayFrom::Start(_)));
                    request.replay_from = replay;
                    let response = cx.send_request(request).block_task().await?;
                    let hive = &response.meta.unwrap()["hive"];
                    assert_eq!(hive["role"], "observer");
                    assert!(hive.get("token").is_none());
                    let mut plans = Vec::new();
                    let mut replayed_ids = std::collections::BTreeSet::new();
                    while plans.len() < native_count {
                        let event = rx.recv().await.unwrap();
                        assert_eq!(event.session_id, id);
                        if let v2::SessionUpdate::PlanUpdate(plan) = event.update {
                            let value = serde_json::to_value(plan).unwrap();
                            if full_snapshot {
                                let meta = &event.meta.as_ref().unwrap()["hive"];
                                assert_eq!(meta["snapshot"], true);
                                assert_eq!(meta["seq"], hive["snapshotThroughSeq"]);
                                assert!(
                                    replayed_ids.insert(
                                        value["plan"]["planId"].as_str().unwrap().to_owned()
                                    ),
                                    "full replay must not duplicate plans"
                                );
                            }
                            plans.push(value);
                        }
                    }
                    if full_snapshot {
                        let expected_ids = hive["planSnapshot"]["plans"]
                            .as_object()
                            .unwrap()
                            .keys()
                            .cloned()
                            .collect();
                        assert_eq!(
                            replayed_ids, expected_ids,
                            "full replay must include every current plan"
                        );
                    }
                    Ok::<_, Error>((hive["planSnapshot"].clone(), plans))
                },
            ),
    )
    .await
    .unwrap()
    .unwrap()
}

#[tokio::test]
async fn plans_outlive_the_replay_ring_and_restart_when_the_agent_replays_them() {
    restore_plans(false).await;
}

#[tokio::test]
async fn a_restart_does_not_invent_plans_that_the_agent_did_not_retain() {
    restore_plans(true).await;
}

async fn restore_plans(forget: bool) {
    let base = std::env::temp_dir().join(format!("hive-plan-recovery-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&base).unwrap();
    let database = base.join("hive/sessions.sqlite3");
    let agent_state = base.join("agent.json");
    let mut config = config_with_agent(
        "mock",
        env!("CARGO_BIN_EXE_hive-test-agent"),
        AgentVersion::V2,
    );
    let profile = config.agents.get_mut("mock").unwrap();
    profile
        .env
        .insert("HIVE_MOCK_STATE".into(), agent_state.display().to_string());
    if forget {
        profile
            .env
            .insert("HIVE_MOCK_FORGET_PLANS".into(), "1".into());
    }
    if std::env::var_os("LLVM_PROFILE_FILE").is_some() {
        // Test agents are killed on shutdown, so only the parent can flush a coverage profile.
        profile.env.insert(
            "LLVM_PROFILE_FILE".into(),
            if cfg!(windows) { "NUL" } else { "/dev/null" }.into(),
        );
    }
    let broker = hive_daemon::Broker::persistent(config.clone(), database.clone()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = hive_daemon::router(broker.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (id, expected) = timeout(
        Duration::from_secs(15),
        Client
            .v2()
            .on_receive_notification(
                async move |event: v2::UpdateSessionNotification, _: V2ConnectionTo<Agent>| {
                    tx.send(event).map_err(Error::into_internal_error)
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(
                HttpClient::new(format!("ws://{address}")).unwrap(),
                async move |cx| {
                    cx.send_request(v2::InitializeRequest::new(
                        ProtocolVersion::V2,
                        v2::Implementation::new("plan-recovery", "1"),
                    ))
                    .block_task()
                    .await?;
                    let cwd = std::env::current_dir().unwrap();
                    let opened = cx
                        .send_request(v2::NewSessionRequest::new(cwd.clone()))
                        .block_task()
                        .await?;
                    let initial = &opened.meta.as_ref().unwrap()["hive"]["planSnapshot"];
                    assert_eq!(initial["support"], "supported");
                    assert_eq!(initial["observation"], "not_observed");
                    cx.send_request(
                        v2::PromptRequest::new(
                            opened.session_id.clone(),
                            vec!["plan-sequence plan-flood usage-fixture".into()],
                        )
                        .meta(controlled(&opened.meta, "plans")),
                    )
                    .block_task()
                    .await?;
                    let mut expected = serde_json::Map::new();
                    loop {
                        let event = rx.recv().await.unwrap();
                        match event.update {
                            v2::SessionUpdate::PlanUpdate(plan) => {
                                let value = serde_json::to_value(plan).unwrap();
                                expected.insert(
                                    value["plan"]["planId"].as_str().unwrap().to_owned(),
                                    value,
                                );
                            }
                            v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(_)) => break,
                            _ => {}
                        }
                    }
                    let expired = cx
                        .send_request(
                            v2::ResumeSessionRequest::new(opened.session_id.clone(), cwd)
                                .replay_from(v2::ReplayFrom::Other(v2::OtherReplayFrom::new(
                                    "_hive_seq",
                                    BTreeMap::from([("seq".into(), 1.into())]),
                                ))),
                        )
                        .block_task()
                        .await
                        .unwrap_err();
                    assert!(
                        expired
                            .data
                            .unwrap()
                            .to_string()
                            .contains("replay cursor unavailable")
                    );
                    Ok::<_, Error>((opened.session_id, serde_json::Value::Object(expected)))
                },
            ),
    )
    .await
    .unwrap()
    .unwrap();
    let (live, _) = plan_attachment(address, id.clone(), None, 0).await;
    assert_eq!(live["plans"], expected);
    let (full, native) = plan_attachment(
        address,
        id.clone(),
        Some(v2::ReplayFrom::Start(v2::ReplayFromStart::new())),
        3,
    )
    .await;
    assert_eq!(full["plans"], expected);
    for plan in native {
        assert_eq!(expected[plan["plan"]["planId"].as_str().unwrap()], plan);
    }
    broker.shutdown().await;
    server.abort();
    let _ = server.await;
    drop(broker);

    let recovered = hive_daemon::Broker::persistent(config, database).unwrap();
    recovered.reconcile().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = hive_daemon::router(recovered.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let (snapshot, _) = plan_attachment(
        address,
        id.clone(),
        Some(v2::ReplayFrom::Start(v2::ReplayFromStart::new())),
        if forget { 0 } else { 3 },
    )
    .await;
    assert_eq!(snapshot["support"], "supported");
    if forget {
        assert_eq!(snapshot["observation"], "not_observed");
        assert_eq!(snapshot["plans"], serde_json::json!({}));
    } else {
        assert_eq!(snapshot["observation"], "observed");
        assert_eq!(snapshot["plans"], expected);
    }
    assert_eq!(
        summaries(address).await["sessions"][0]["sessionId"],
        id.to_string()
    );
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&agent_state).unwrap()).unwrap();
    assert_eq!(saved["sessions"].as_object().unwrap().len(), 1);
    assert_eq!(
        saved["next"], 2,
        "observing must not prompt or create a replacement session"
    );
    recovered.shutdown().await;
    server.abort();
    let _ = server.await;
    drop(recovered);
    #[cfg(not(windows))]
    std::fs::remove_dir_all(base).unwrap();
    #[cfg(windows)]
    {
        // Aborted workers can retain SQLite handles until the test runtime exits on Windows.
        // The runner removes this unique directory after those handles have been released.
        let _ = std::fs::remove_dir_all(base);
    }
}

#[tokio::test]
async fn explicit_and_default_profiles_share_one_broker_across_client_transports() {
    let binary = env!("CARGO_BIN_EXE_hive-test-agent");
    let mut config = config_with_agent("pi", binary, AgentVersion::V2);
    let mut other = config.agents["pi"].clone();
    other
        .env
        .insert("HIVE_MOCK_LABEL".into(), "OpenCode ".into());
    config.agents.insert("opencode".into(), other);
    let (address, server) = start_gateway(config).await;
    let ws = format!("ws://{address}");
    let http = format!("http://{address}");
    let (pi, opencode, other_pi) = tokio::join!(
        create_and_prompt(&ws, None, "one", "Echo: one"),
        create_and_prompt(&http, Some("opencode"), "two", "OpenCode Echo: two"),
        create_and_prompt(&ws, Some("pi"), "three", "Echo: three"),
    );
    assert_ne!(pi, opencode);
    assert_ne!(pi, other_pi);
    let status: serde_json::Value = reqwest::get(format!("http://{address}/v1/status"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["sessions"], 3);
    assert_eq!(status["activeByAgent"]["pi"], 2);
    assert_eq!(status["activeByAgent"]["opencode"], 1);
    server.abort();
}

async fn session_error(url: String, agent: Option<&str>) -> String {
    let agent = agent.map(str::to_string);
    Client
        .v2()
        .connect_with(HttpClient::new(url).unwrap(), async move |cx| {
            cx.send_request(v2::InitializeRequest::new(
                ProtocolVersion::V2,
                v2::Implementation::new("errors", "1"),
            ))
            .block_task()
            .await?;
            let mut request = v2::NewSessionRequest::new(
                std::env::current_dir().map_err(Error::into_internal_error)?,
            );
            if let Some(agent) = agent {
                let mut meta = v2::Meta::new();
                meta.insert("hive".into(), serde_json::json!({"agent": agent}));
                request = request.meta(meta);
            }
            let error = cx.send_request(request).block_task().await.unwrap_err();
            Ok(error.data.unwrap_or_default().to_string())
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn missing_defaults_and_unknown_agents_report_selection_remedies_on_both_transports() {
    let (address, server) = start_gateway(Config::default()).await;
    for transport in ["ws", "http"] {
        let url = format!("{transport}://{address}");
        assert!(
            session_error(url.clone(), None)
                .await
                .contains("default agent")
        );
        assert!(
            session_error(url, Some("missing"))
                .await
                .contains("unknown agent profile")
        );
    }
    server.abort();
}

#[tokio::test]
async fn a_broken_agent_and_protocol_mismatch_do_not_register_sessions() {
    let mut config = config_with_agent("missing", "/does/not/exist/hive-agent", AgentVersion::V2);
    let mut mismatch = config_with_agent(
        "mismatch",
        &format!("{} v1", env!("CARGO_BIN_EXE_hive-test-agent")),
        AgentVersion::V2,
    );
    config.agents.append(&mut mismatch.agents);
    let (address, server) = start_gateway(config).await;
    let url = format!("http://{address}");
    assert!(
        session_error(url.clone(), Some("missing"))
            .await
            .contains("agent")
    );
    assert!(session_error(url, Some("mismatch")).await.contains("agent"));
    let status: serde_json::Value = reqwest::get(format!("http://{address}/v1/status"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["sessions"], 0);
    server.abort();
}

#[tokio::test]
async fn declared_http_and_websocket_agents_are_reached_through_the_sdk() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let downstream = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let agent = || {
            Agent
                .v2()
                .name("remote-mock")
                .on_receive_request(
                    async |request: v2::InitializeRequest,
                           responder: Responder<v2::InitializeResponse>,
                           _: V2ConnectionTo<Client>| {
                        responder.respond(
                            v2::InitializeResponse::new(
                                request.protocol_version,
                                v2::Implementation::new("remote-mock", "1"),
                            )
                            .capabilities(
                                v2::AgentCapabilities::new()
                                    .session(v2::SessionCapabilities::new()),
                            ),
                        )
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    async |_: v2::NewSessionRequest,
                           responder: Responder<v2::NewSessionResponse>,
                           _: V2ConnectionTo<Client>| {
                        responder.respond(v2::NewSessionResponse::new(v2::SessionId::new(
                            "remote-session",
                        )))
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    async |request: v2::PromptRequest,
                           responder: Responder<v2::PromptResponse>,
                           cx: V2ConnectionTo<Client>| {
                        responder
                            .respond(v2::PromptResponse::new(v2::MessageId::new("remote-user")))?;
                        cx.send_notification(v2::UpdateSessionNotification::new(
                            request.session_id.clone(),
                            v2::SessionUpdate::AgentMessageChunk(v2::ContentChunk::new(
                                "Remote Echo".into(),
                                v2::MessageId::new("remote-agent"),
                            )),
                        ))?;
                        cx.send_notification(v2::UpdateSessionNotification::new(
                            request.session_id,
                            v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(
                                v2::IdleStateUpdate::new(),
                            )),
                        ))
                    },
                    agent_client_protocol::on_receive_request!(),
                )
        };
        axum::serve(
            listener,
            agent_client_protocol_http::AcpHttpServer::new(agent).into_router(),
        )
        .await
        .unwrap();
    });
    let mut config = Config::default();
    for (id, endpoint) in [
        ("remote-http", format!("http://{downstream}")),
        ("remote-ws", format!("ws://{downstream}")),
    ] {
        config.agents.insert(
            id.into(),
            Profile {
                name: None,
                command: None,
                args: vec![],
                env: BTreeMap::new(),
                endpoint: Some(endpoint),
                headers: BTreeMap::new(),
                versions: vec![AgentVersion::V2],
                pricing: Default::default(),
                kind: Default::default(),
                username: None,
                password_env: None,
            },
        );
    }
    let (address, gateway) = start_gateway(config).await;
    for id in ["remote-http", "remote-ws"] {
        create_and_prompt(
            &format!("http://{address}"),
            Some(id),
            "hello",
            "Remote Echo",
        )
        .await;
    }
    gateway.abort();
    server.abort();
}

#[tokio::test]
async fn disconnecting_one_client_does_not_lose_its_resumable_agent_session() {
    let config = config_with_agent(
        "mock",
        env!("CARGO_BIN_EXE_hive-test-agent"),
        AgentVersion::V2,
    );
    let (address, server) = start_gateway(config).await;
    let id = create_and_prompt(&format!("ws://{address}"), None, "before", "Echo: before").await;
    let status: serde_json::Value = reqwest::get(format!("http://{address}/v1/status"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["sessions"], 1);
    let (tx, mut rx) = mpsc::unbounded_channel();
    Client
        .v2()
        .on_receive_notification(
            async move |update: v2::UpdateSessionNotification, _: V2ConnectionTo<Agent>| {
                tx.send(update).map_err(Error::into_internal_error)
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(
            HttpClient::new(format!("http://{address}")).unwrap(),
            async move |cx| {
                cx.send_request(v2::InitializeRequest::new(
                    ProtocolVersion::V2,
                    v2::Implementation::new("resume-test", "1"),
                ))
                .block_task()
                .await?;
                let cwd = std::env::current_dir().map_err(Error::into_internal_error)?;
                let resumed = cx
                    .send_request(v2::ResumeSessionRequest::new(id.clone(), cwd))
                    .block_task()
                    .await?;
                cx.send_request(
                    v2::PromptRequest::new(id.clone(), vec!["after".into()])
                        .meta(controlled(&resumed.meta, "after")),
                )
                .block_task()
                .await?;
                let saw_reply_and_idle = timeout(Duration::from_secs(5), async {
                    let mut saw_reply = false;
                    let mut saw_idle = false;
                    while let Some(update) = rx.recv().await {
                        if update.session_id != id {
                            continue;
                        }
                        match update.update {
                            v2::SessionUpdate::AgentMessageChunk(chunk)
                                if matches!(chunk.content, v2::ContentBlock::Text(ref text) if text.text == "Echo: after") =>
                            {
                                saw_reply = true;
                            }
                            v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(_)) => {
                                saw_idle = true;
                            }
                            _ => {}
                        }
                        if saw_reply && saw_idle {
                            return true;
                        }
                    }
                    false
                })
                .await
                .map_err(Error::into_internal_error)?;
                assert!(saw_reply_and_idle);
                cx.send_request(v2::CloseSessionRequest::new(id).meta(resumed.meta.unwrap()))
                    .block_task()
                    .await?;
                Ok(())
            },
        )
        .await
        .unwrap();
    server.abort();
}

#[tokio::test]
async fn a_cross_transport_handoff_fences_the_old_controller_and_replays_once() {
    let config = config_with_agent(
        "mock",
        env!("CARGO_BIN_EXE_hive-test-agent"),
        AgentVersion::V2,
    );
    let (address, server) = start_gateway(config).await;
    let (created_tx, created_rx) = oneshot::channel();
    let (handoff_tx, handoff_rx) = oneshot::channel();
    let old = tokio::spawn(async move {
        let (updates, mut received) = mpsc::unbounded_channel();
        Client
            .v2()
            .on_receive_notification(
                async move |update: v2::UpdateSessionNotification, _: V2ConnectionTo<Agent>| {
                    updates.send(update).map_err(Error::into_internal_error)
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(
                HttpClient::new(format!("ws://{address}")).unwrap(),
                async move |cx| {
                    cx.send_request(v2::InitializeRequest::new(
                        ProtocolVersion::V2,
                        v2::Implementation::new("handoff-a", "1"),
                    ))
                    .block_task()
                    .await?;
                    let cwd = std::env::current_dir().unwrap();
                    let created = cx
                        .send_request(v2::NewSessionRequest::new(cwd))
                        .block_task()
                        .await?;
                    let id = created.session_id;
                    let meta = created.meta;
                    let prompt = v2::PromptRequest::new(id.clone(), vec!["once".into()])
                        .meta(controlled(&meta, "same-key"));
                    let response = cx.send_request(prompt).block_task().await?;
                    let mut running = false;
                    while let Some(update) = received.recv().await {
                        if update.session_id != id {
                            continue;
                        }
                        if matches!(
                            update.update,
                            v2::SessionUpdate::StateUpdate(v2::StateUpdate::Running(_))
                        ) {
                            running = true;
                        }
                        if running
                            && matches!(
                                update.update,
                                v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(_))
                            )
                        {
                            break;
                        }
                    }
                    created_tx
                        .send((id.clone(), meta.clone(), response.message_id))
                        .unwrap();
                    handoff_rx.await.unwrap();
                    let stale = cx
                        .send_request(
                            v2::PromptRequest::new(id.clone(), vec!["stale".into()])
                                .meta(controlled(&meta, "stale-key")),
                        )
                        .block_task()
                        .await
                        .unwrap_err();
                    assert!(stale.data.unwrap().to_string().contains("stale"));
                    let stale_close = cx
                        .send_request(v2::CloseSessionRequest::new(id).meta(meta.unwrap()))
                        .block_task()
                        .await
                        .unwrap_err();
                    assert!(stale_close.data.unwrap().to_string().contains("stale"));
                    Ok(())
                },
            )
            .await
    });
    let (id, _meta, original) = match timeout(Duration::from_secs(10), created_rx).await.unwrap() {
        Ok(value) => value,
        Err(_) => panic!("creator exited: {:?}", old.await),
    };
    let old_owner = summaries(address).await["sessions"][0]["controlOwner"]
        .as_str()
        .unwrap()
        .to_owned();
    let (tx, mut rx) = mpsc::unbounded_channel();
    timeout(
        Duration::from_secs(10),
        Client
            .v2()
            .on_receive_notification(
                async move |update: v2::UpdateSessionNotification, _: V2ConnectionTo<Agent>| {
                    tx.send(update).map_err(Error::into_internal_error)
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(
                HttpClient::new(format!("http://{address}")).unwrap(),
                async move |cx| {
                    cx.send_request(v2::InitializeRequest::new(
                        ProtocolVersion::V2,
                        v2::Implementation::new("handoff-b", "1"),
                    ))
                    .block_task()
                    .await?;
                    let cwd = std::env::current_dir().unwrap();
                    let invalid = v2::ReplayFrom::Other(v2::OtherReplayFrom::new(
                        "_hive_seq",
                        BTreeMap::from([("seq".into(), serde_json::json!(99999))]),
                    ));
                    assert!(
                        cx.send_request(
                            v2::ResumeSessionRequest::new(id.clone(), cwd.clone())
                                .replay_from(invalid)
                        )
                        .block_task()
                        .await
                        .is_err()
                    );
                    let resumed = cx
                        .send_request(
                            v2::ResumeSessionRequest::new(id.clone(), cwd)
                                .replay_from(v2::ReplayFrom::Start(v2::ReplayFromStart::new())),
                        )
                        .block_task()
                        .await?;
                    assert_eq!(resumed.meta.as_ref().unwrap()["hive"]["generation"], 2);
                    let snapshot = summaries(address).await;
                    assert_eq!(snapshot["sessions"][0]["generation"], 2);
                    assert_ne!(snapshot["sessions"][0]["controlOwner"], old_owner);
                    assert_eq!(snapshot["sessions"][0]["title"], "once");
                    assert!(snapshot["sessions"][0].get("token").is_none());
                    handoff_tx.send(()).unwrap();
                    let mut seq = resumed.meta.as_ref().unwrap()["hive"]["snapshotThroughSeq"]
                        .as_u64()
                        .unwrap();
                    let mut snapshot_messages = 0;
                    while seq + 1
                        < resumed.meta.as_ref().unwrap()["hive"]["nextSeq"]
                            .as_u64()
                            .unwrap()
                    {
                        let update = timeout(Duration::from_secs(5), rx.recv())
                            .await
                            .unwrap()
                            .unwrap();
                        let hive = &update.meta.as_ref().unwrap()["hive"];
                        if hive["snapshot"] == true {
                            assert!(matches!(
                                update.update,
                                v2::SessionUpdate::UserMessage(_)
                                    | v2::SessionUpdate::AgentMessage(_)
                            ));
                            snapshot_messages += 1;
                            continue;
                        }
                        let next = hive["seq"].as_u64().unwrap();
                        assert_eq!(
                            next,
                            seq + 1,
                            "live events following a snapshot remain contiguous"
                        );
                        seq = next;
                    }
                    assert_eq!(snapshot_messages, 2);
                    let retry = cx
                        .send_request(
                            v2::PromptRequest::new(id.clone(), vec!["once".into()])
                                .meta(controlled(&resumed.meta, "same-key")),
                        )
                        .block_task()
                        .await?;
                    assert_eq!(retry.message_id, original);
                    assert!(
                        cx.send_request(
                            v2::PromptRequest::new(id.clone(), vec!["changed".into()])
                                .meta(controlled(&resumed.meta, "same-key"))
                        )
                        .block_task()
                        .await
                        .is_err()
                    );
                    let next = cx
                        .send_request(
                            v2::PromptRequest::new(id.clone(), vec!["second".into()])
                                .meta(controlled(&resumed.meta, "next-key")),
                        )
                        .block_task()
                        .await?;
                    assert_ne!(next.message_id, original);
                    while let Some(update) = rx.recv().await {
                        if update.session_id == id
                            && matches!(
                                update.update,
                                v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(_))
                            )
                        {
                            break;
                        }
                    }
                    cx.send_request(v2::CloseSessionRequest::new(id).meta(resumed.meta.unwrap()))
                        .block_task()
                        .await?;
                    Ok(())
                },
            ),
    )
    .await
    .unwrap()
    .unwrap();
    old.await.unwrap().unwrap();
    server.abort();
}

#[tokio::test]
async fn a_disconnect_and_handoff_during_generation_do_not_restart_the_prompt() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let downstream = listener.local_addr().unwrap();
    let (started_tx, started_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let pause = std::sync::Arc::new(tokio::sync::Mutex::new(Some((started_tx, release_rx))));
    let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let calls = count.clone();
    let agent = tokio::spawn(async move {
        let server = move || {
            let pause = pause.clone();
            let calls = calls.clone();
            Agent
                .v2()
                .name("paused")
                .on_receive_request(
                    async |r: v2::InitializeRequest,
                           responder: Responder<v2::InitializeResponse>,
                           _: V2ConnectionTo<Client>| {
                        responder.respond(
                            v2::InitializeResponse::new(
                                r.protocol_version,
                                v2::Implementation::new("paused", "1"),
                            )
                            .capabilities(
                                v2::AgentCapabilities::new()
                                    .session(v2::SessionCapabilities::new()),
                            ),
                        )
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    async |_: v2::NewSessionRequest,
                           responder: Responder<v2::NewSessionResponse>,
                           _: V2ConnectionTo<Client>| {
                        responder.respond(v2::NewSessionResponse::new(v2::SessionId::new(
                            "downstream-id",
                        )))
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    async move |r: v2::PromptRequest,
                                responder: Responder<v2::PromptResponse>,
                                cx: V2ConnectionTo<Client>| {
                        let index = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        responder.respond(v2::PromptResponse::new(v2::MessageId::new(format!(
                            "user-{index}"
                        ))))?;
                        cx.send_notification(v2::UpdateSessionNotification::new(
                            r.session_id.clone(),
                            v2::SessionUpdate::StateUpdate(v2::StateUpdate::Running(
                                v2::RunningStateUpdate::new(),
                            )),
                        ))?;
                        if let Some((started, release)) = pause.lock().await.take() {
                            let _ = started.send(());
                            let _ = release.await;
                        }
                        cx.send_notification(v2::UpdateSessionNotification::new(
                            r.session_id,
                            v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(
                                v2::IdleStateUpdate::new(),
                            )),
                        ))
                    },
                    agent_client_protocol::on_receive_request!(),
                )
        };
        axum::serve(
            listener,
            agent_client_protocol_http::AcpHttpServer::new(server).into_router(),
        )
        .await
        .unwrap();
    });
    let mut config = Config {
        default_agent: Some("paused".into()),
        ..Config::default()
    };
    config.agents.insert(
        "paused".into(),
        Profile {
            name: None,
            command: None,
            args: vec![],
            env: BTreeMap::new(),
            endpoint: Some(format!("http://{downstream}")),
            headers: BTreeMap::new(),
            versions: vec![AgentVersion::V2],
            pricing: Default::default(),
            kind: Default::default(),
            username: None,
            password_env: None,
        },
    );
    let (address, gateway) = start_gateway(config).await;
    let id = Client
        .v2()
        .connect_with(
            HttpClient::new(format!("ws://{address}")).unwrap(),
            async |cx| {
                cx.send_request(v2::InitializeRequest::new(
                    ProtocolVersion::V2,
                    v2::Implementation::new("first", "1"),
                ))
                .block_task()
                .await?;
                let created = cx
                    .send_request(v2::NewSessionRequest::new(std::env::current_dir().unwrap()))
                    .block_task()
                    .await?;
                cx.send_request(
                    v2::PromptRequest::new(created.session_id.clone(), vec!["once".into()])
                        .meta(controlled(&created.meta, "once")),
                )
                .block_task()
                .await?;
                Ok(created.session_id)
            },
        )
        .await
        .unwrap();
    timeout(Duration::from_secs(5), started_rx)
        .await
        .unwrap()
        .unwrap();
    let (tx, mut updates) = mpsc::unbounded_channel();
    timeout(
        Duration::from_secs(10),
        Client
            .v2()
            .on_receive_notification(
                async move |update: v2::UpdateSessionNotification, _: V2ConnectionTo<Agent>| {
                    tx.send(update).map_err(Error::into_internal_error)
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(
                HttpClient::new(format!("http://{address}")).unwrap(),
                async move |cx| {
                    cx.send_request(v2::InitializeRequest::new(
                        ProtocolVersion::V2,
                        v2::Implementation::new("second", "1"),
                    ))
                    .block_task()
                    .await?;
                    let resumed = cx
                        .send_request(
                            v2::ResumeSessionRequest::new(
                                id.clone(),
                                std::env::current_dir().unwrap(),
                            )
                            .replay_from(v2::ReplayFrom::Start(v2::ReplayFromStart::new())),
                        )
                        .block_task()
                        .await?;
                    assert_eq!(resumed.meta.as_ref().unwrap()["hive"]["running"], true);
                    assert!(
                        cx.send_request(
                            v2::PromptRequest::new(id.clone(), vec!["two".into()])
                                .meta(controlled(&resumed.meta, "two"))
                        )
                        .block_task()
                        .await
                        .is_err()
                    );
                    release_tx.send(()).unwrap();
                    while let Some(update) = updates.recv().await {
                        if matches!(
                            update.update,
                            v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(_))
                        ) {
                            break;
                        }
                    }
                    cx.send_request(
                        v2::PromptRequest::new(id.clone(), vec!["two".into()])
                            .meta(controlled(&resumed.meta, "two")),
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
    assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 2);
    gateway.abort();
    agent.abort();
}

#[tokio::test]
async fn simultaneous_resume_requests_are_serialized_into_distinct_owners() {
    let config = config_with_agent(
        "mock",
        env!("CARGO_BIN_EXE_hive-test-agent"),
        AgentVersion::V2,
    );
    let (address, server) = start_gateway(config).await;
    let id = create_and_prompt(&format!("ws://{address}"), None, "initial", "Echo: initial").await;
    // The creator's disconnect legitimately fences its generation before either racer attaches.
    let generation = timeout(Duration::from_secs(5), async {
        loop {
            let catalog = summaries(address).await;
            let session = &catalog["sessions"][0];
            if session["controlOwner"].is_null() {
                break session["generation"].as_u64().unwrap();
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
    let takeover = |url: String, id: v2::SessionId, gate: std::sync::Arc<tokio::sync::Barrier>| async move {
        Client
            .v2()
            .connect_with(HttpClient::new(url).unwrap(), async move |cx| {
                cx.send_request(v2::InitializeRequest::new(
                    ProtocolVersion::V2,
                    v2::Implementation::new("racer", "1"),
                ))
                .block_task()
                .await?;
                gate.wait().await;
                let resumed = cx
                    .send_request(v2::ResumeSessionRequest::new(
                        id,
                        std::env::current_dir().unwrap(),
                    ))
                    .block_task()
                    .await?;
                let generation = resumed.meta.unwrap()["hive"]["generation"]
                    .as_u64()
                    .unwrap();
                // Neither racer may disconnect and rotate control before the other resumes.
                gate.wait().await;
                Ok(generation)
            })
            .await
            .unwrap()
    };
    let (first, second) = timeout(Duration::from_secs(10), async {
        tokio::join!(
            takeover(format!("ws://{address}"), id.clone(), barrier.clone()),
            takeover(format!("http://{address}"), id, barrier),
        )
    })
    .await
    .unwrap();
    assert_eq!(
        [first.min(second), first.max(second)],
        [generation + 1, generation + 2]
    );
    server.abort();
}

#[tokio::test]
async fn a_websocket_client_reaches_the_mock_agent_through_hive() {
    exercise_gateway("ws", AgentVersion::V2).await;
}

#[tokio::test]
async fn an_http_sse_client_reaches_the_same_broker() {
    exercise_gateway("http", AgentVersion::V2).await;
}

#[tokio::test]
async fn a_v1_stdio_agent_is_adapted_to_the_v2_websocket_client() {
    exercise_gateway("ws", AgentVersion::V1).await;
}

#[tokio::test]
async fn observers_keep_streaming_after_pilot_relinquishes_and_can_take_control() {
    let config = config_with_agent(
        "mock",
        env!("CARGO_BIN_EXE_hive-test-agent"),
        AgentVersion::V2,
    );
    let (address, gateway) = start_gateway(config).await;
    let (opened_tx, opened_rx) = oneshot::channel();
    let (command_tx, mut commands) = mpsc::unbounded_channel::<&'static str>();
    let first = tokio::spawn(async move {
        Client
            .v2()
            .connect_with(
                HttpClient::new(format!("ws://{address}")).unwrap(),
                async move |cx| {
                    cx.send_request(v2::InitializeRequest::new(
                        ProtocolVersion::V2,
                        v2::Implementation::new("pilot", "1"),
                    ))
                    .block_task()
                    .await?;
                    let created = cx
                        .send_request(v2::NewSessionRequest::new(std::env::current_dir().unwrap()))
                        .block_task()
                        .await?;
                    let id = created.session_id;
                    let mut owner = created.meta;
                    opened_tx.send(id.clone()).unwrap();
                    while let Some(command) = commands.recv().await {
                        match command {
                            "prompt" => {
                                cx.send_request(
                                    v2::PromptRequest::new(
                                        id.clone(),
                                        vec!["observer sees this".into()],
                                    )
                                    .meta(controlled(&owner, "observer-prompt")),
                                )
                                .block_task()
                                .await?;
                            }
                            "release" => {
                                let mut meta = owner.clone().unwrap();
                                meta.get_mut("hive").unwrap()["mode"] =
                                    serde_json::json!("relinquish");
                                let response = cx
                                    .send_request(
                                        v2::ResumeSessionRequest::new(
                                            id.clone(),
                                            std::env::current_dir().unwrap(),
                                        )
                                        .meta(meta),
                                    )
                                    .block_task()
                                    .await?;
                                assert_eq!(
                                    response.meta.as_ref().unwrap()["hive"]["role"],
                                    "observer"
                                );
                                assert!(
                                    response.meta.as_ref().unwrap()["hive"]
                                        .get("token")
                                        .is_none()
                                );
                                assert!(
                                    cx.send_request(
                                        v2::PromptRequest::new(id.clone(), vec!["stale".into()])
                                            .meta(controlled(&owner, "stale"))
                                    )
                                    .block_task()
                                    .await
                                    .is_err()
                                );
                                owner = response.meta;
                            }
                            _ => break,
                        }
                    }
                    Ok(())
                },
            )
            .await
    });
    let id = opened_rx.await.unwrap();
    let (updates_tx, mut updates) = mpsc::unbounded_channel();
    timeout(
        Duration::from_secs(10),
        Client
            .v2()
            .on_receive_notification(
                async move |update: v2::UpdateSessionNotification, _: V2ConnectionTo<Agent>| {
                    updates_tx.send(update).map_err(Error::into_internal_error)
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(
                HttpClient::new(format!("http://{address}")).unwrap(),
                async move |cx| {
                    cx.send_request(v2::InitializeRequest::new(
                        ProtocolVersion::V2,
                        v2::Implementation::new("observer", "1"),
                    ))
                    .block_task()
                    .await?;
                    let cwd = std::env::current_dir().unwrap();
                    let mut meta = v2::Meta::new();
                    meta.insert("hive".into(), serde_json::json!({"mode": "observe"}));
                    let observed = cx
                        .send_request(
                            v2::ResumeSessionRequest::new(id.clone(), cwd.clone()).meta(meta),
                        )
                        .block_task()
                        .await?;
                    assert_eq!(observed.meta.as_ref().unwrap()["hive"]["role"], "observer");
                    assert!(
                        observed.meta.as_ref().unwrap()["hive"]
                            .get("token")
                            .is_none()
                    );
                    command_tx.send("prompt").unwrap();
                    let mut saw_text = false;
                    while let Some(update) = updates.recv().await {
                        if matches!(update.update, v2::SessionUpdate::AgentMessageChunk(_)) {
                            saw_text = true;
                        }
                        if saw_text
                            && matches!(
                                update.update,
                                v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(_))
                            )
                        {
                            break;
                        }
                    }
                    command_tx.send("release").unwrap();
                    while let Some(update) = updates.recv().await {
                        if let v2::SessionUpdate::Other(other) = update.update
                            && other.session_update == "_hive_control"
                            && other.fields["pilot"] == false
                        {
                            break;
                        }
                    }
                    let released = summaries(address).await;
                    assert!(released["sessions"][0]["controlOwner"].is_null());
                    assert_eq!(released["sessions"][0]["observers"], 2);
                    let resumed = cx
                        .send_request(v2::ResumeSessionRequest::new(id.clone(), cwd))
                        .block_task()
                        .await?;
                    assert_eq!(resumed.meta.as_ref().unwrap()["hive"]["role"], "pilot");
                    assert!(
                        summaries(address).await["sessions"][0]["controlOwner"]
                            .as_str()
                            .is_some()
                    );
                    cx.send_request(
                        v2::PromptRequest::new(id, vec!["new pilot".into()])
                            .meta(controlled(&resumed.meta, "new-pilot")),
                    )
                    .block_task()
                    .await?;
                    command_tx.send("done").unwrap();
                    Ok(())
                },
            ),
    )
    .await
    .unwrap()
    .unwrap();
    first.await.unwrap().unwrap();
    gateway.abort();
}

#[tokio::test]
async fn permission_and_form_and_url_elicitation_reach_the_current_pilot() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let downstream = listener.local_addr().unwrap();
    let agent = tokio::spawn(async move {
        let make_agent = || {
            Agent
                .v2()
                .name("requests")
                .on_receive_request(
                    async |r: v2::InitializeRequest,
                           responder: Responder<v2::InitializeResponse>,
                           _: V2ConnectionTo<Client>| {
                        responder.respond(
                            v2::InitializeResponse::new(
                                r.protocol_version,
                                v2::Implementation::new("requests", "1"),
                            )
                            .capabilities(
                                v2::AgentCapabilities::new()
                                    .session(v2::SessionCapabilities::new()),
                            ),
                        )
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    async |_: v2::NewSessionRequest,
                           responder: Responder<v2::NewSessionResponse>,
                           _: V2ConnectionTo<Client>| {
                        responder.respond(v2::NewSessionResponse::new(v2::SessionId::new(
                            "downstream",
                        )))
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    async |r: v2::PromptRequest,
                           responder: Responder<v2::PromptResponse>,
                           cx: V2ConnectionTo<Client>| {
                        responder.respond(v2::PromptResponse::new(v2::MessageId::new("user")))?;
                        tokio::spawn(async move {
                            let result: agent_client_protocol::Result<()> = async {
                                let choice = cx
                                    .send_request(v2::RequestPermissionRequest::new(
                                        r.session_id.clone(),
                                        "Run tool",
                                        vec![v2::PermissionOption::new(
                                            "allow",
                                            "Allow once",
                                            v2::PermissionOptionKind::AllowOnce,
                                        )],
                                    ))
                                    .block_task()
                                    .await?;
                                assert!(matches!(
                                    choice.outcome,
                                    v2::RequestPermissionOutcome::Selected(_)
                                ));
                                let scope = v2::ElicitationSessionScope::new(r.session_id.clone());
                                let form = v2::ElicitationFormMode::new(
                                    scope,
                                    v2::ElicitationSchema::new(),
                                );
                                let answer = cx
                                    .send_request(v2::CreateElicitationRequest::new(form, "Name?"))
                                    .block_task()
                                    .await?;
                                assert!(matches!(answer.action, v2::ElicitationAction::Accept(_)));
                                let url = v2::ElicitationUrlMode::new(
                                    v2::ElicitationSessionScope::new(r.session_id.clone()),
                                    "url-1",
                                    "https://example.com/auth",
                                );
                                let answer = cx
                                    .send_request(v2::CreateElicitationRequest::new(
                                        url,
                                        "Authenticate",
                                    ))
                                    .block_task()
                                    .await?;
                                assert!(matches!(answer.action, v2::ElicitationAction::Accept(_)));
                                cx.send_notification(v2::UpdateSessionNotification::new(
                                    r.session_id,
                                    v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(
                                        v2::IdleStateUpdate::new(),
                                    )),
                                ))
                            }
                            .await;
                            result.unwrap();
                        });
                        Ok(())
                    },
                    agent_client_protocol::on_receive_request!(),
                )
        };
        axum::serve(
            listener,
            agent_client_protocol_http::AcpHttpServer::new(make_agent).into_router(),
        )
        .await
        .unwrap();
    });
    let mut config = Config {
        default_agent: Some("requests".into()),
        ..Config::default()
    };
    config.agents.insert(
        "requests".into(),
        Profile {
            name: None,
            command: None,
            args: vec![],
            env: BTreeMap::new(),
            endpoint: Some(format!("http://{downstream}")),
            headers: BTreeMap::new(),
            versions: vec![AgentVersion::V2],
            pricing: Default::default(),
            kind: Default::default(),
            username: None,
            password_env: None,
        },
    );
    let (address, gateway) = start_gateway(config).await;
    let (tx, mut updates) = mpsc::unbounded_channel();
    timeout(Duration::from_secs(10), Client.v2()
        .on_receive_notification(async move |update: v2::UpdateSessionNotification, _: V2ConnectionTo<Agent>| {
            tx.send(update).map_err(Error::into_internal_error)
        }, agent_client_protocol::on_receive_notification!())
        .on_receive_request(async |request: v2::RequestPermissionRequest, responder: Responder<v2::RequestPermissionResponse>, _: V2ConnectionTo<Agent>| {
            assert!(request.session_id.to_string().starts_with("hive-"));
            responder.respond(v2::RequestPermissionResponse::new(v2::RequestPermissionOutcome::Selected(
                v2::SelectedPermissionOutcome::new("allow"))))
        }, agent_client_protocol::on_receive_request!())
        .on_receive_request(async |request: v2::CreateElicitationRequest, responder: Responder<v2::CreateElicitationResponse>, _: V2ConnectionTo<Agent>| {
            assert!(matches!(request.scope(), v2::ElicitationScope::Session(scope) if scope.session_id.to_string().starts_with("hive-")));
            responder.respond(v2::CreateElicitationResponse::new(v2::ElicitationAction::Accept(v2::ElicitationAcceptAction::new())))
        }, agent_client_protocol::on_receive_request!())
        .connect_with(HttpClient::new(format!("ws://{address}")).unwrap(), async move |cx| {
            cx.send_request(v2::InitializeRequest::new(ProtocolVersion::V2, v2::Implementation::new("pilot", "1")).capabilities(v2::ClientCapabilities::new().elicitation(v2::ElicitationCapabilities::new().form(v2::ElicitationFormCapabilities::new()).url(v2::ElicitationUrlCapabilities::new())))).block_task().await?;
            let opened = cx.send_request(v2::NewSessionRequest::new(std::env::current_dir().unwrap())).block_task().await?;
            cx.send_request(v2::PromptRequest::new(opened.session_id.clone(), vec!["interact".into()]).meta(controlled(&opened.meta, "requests"))).block_task().await?;
            while let Some(update) = updates.recv().await {
                if matches!(update.update, v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(_))) { break; }
            }
            Ok(())
        })).await.unwrap().unwrap();
    gateway.abort();
    agent.abort();
}
