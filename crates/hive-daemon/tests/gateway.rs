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

fn controlled(meta: &Option<v2::Meta>, key: &str) -> v2::Meta {
    let mut value = meta.clone().unwrap();
    value.get_mut("hive").unwrap()["key"] = serde_json::json!(key);
    value
}

async fn summaries(address: SocketAddr) -> serde_json::Value {
    reqwest::get(format!("http://{address}/hive/v1/sessions"))
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap()
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
    let status: serde_json::Value = reqwest::get(format!("http://{address}/hive/v1/status"))
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
    let mut events = reqwest::get(format!("http://{address}/hive/v1/sessions/events"))
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let mut pending = String::new();
    let initial = next_revision(&mut events, &mut pending).await;
    let health: serde_json::Value = reqwest::get(format!("http://{address}/hive/v1/health"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(health["status"], "ok");
    let web = reqwest::get(format!("http://{address}/hive/v1/web"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(web.contains("/hive/v1/web/app.js"));
    let provider: serde_json::Value = reqwest::get(format!("http://{address}/hive/v1/providers"))
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
            let mut reconnected = reqwest::get(format!("http://{address}/hive/v1/sessions/events"))
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
        "INSERT INTO prompts (session_id,key,content) VALUES (?1,'interrupted','[]')",
        [id.to_string()],
    )
    .unwrap();
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
                    .send_request(
                        v2::PromptRequest::new(id.clone(), vec!["hello".into()]).meta(meta),
                    )
                    .block_task()
                    .await;
                assert!(result.is_err(), "an interrupted key must never be resent");
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
                    let mut last = 0;
                    while let Some(event) = rx.recv().await {
                        let seq = event.meta.as_ref().unwrap()["hive"]["seq"].as_u64().unwrap();
                        assert!(seq > last);
                        last = seq;
                        if let v2::SessionUpdate::Other(other) = event.update
                            && other.session_update == "_hive_activity" {
                                kinds.push(other.fields["kind"].as_str().unwrap().to_owned());
                                if other.fields["kind"] == "task" { break; }
                            }
                    }
                    assert!(kinds.windows(2).any(|window| window == ["tool", "plan"]));
                    assert_eq!(kinds.last().unwrap(), "task");
                    Ok(())
                }).await.unwrap();
            });
            attached.await.map_err(Error::into_internal_error)?;
            cx.send_request(v2::PromptRequest::new(opened.session_id.clone(), vec!["activity-sequence".into()])
                .meta(controlled(&opened.meta, "activity-prompt"))).block_task().await?;
            while let Some(event) = observed_rx.recv().await {
                if let v2::SessionUpdate::Other(other) = &event.update
                    && other.session_update == "_hive_activity" && other.fields["kind"] == "task" { break; }
            }
            observer.await.map_err(Error::into_internal_error)?;
            let mut meta = v2::Meta::new();
            meta.insert("hive".into(), serde_json::json!({"mode":"observe"}));
            let response = cx.send_request(v2::ResumeSessionRequest::new(opened.session_id, std::env::current_dir().unwrap()).meta(meta)).block_task().await?;
            let snapshot = &response.meta.unwrap()["hive"]["activitySnapshot"];
            assert_eq!(snapshot["foreground"], "idle");
            assert_eq!(snapshot["tools"]["mock-tool"]["status"], "completed");
            assert_eq!(snapshot["tasks"]["background"]["status"], "completed");
            Ok(())
        })).await.unwrap().unwrap();
    server.abort();
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
    let status: serde_json::Value = reqwest::get(format!("http://{address}/hive/v1/status"))
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
    let status: serde_json::Value = reqwest::get(format!("http://{address}/hive/v1/status"))
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
    let status: serde_json::Value = reqwest::get(format!("http://{address}/hive/v1/status"))
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
                    let mut seq = 0;
                    while seq + 1
                        < resumed.meta.as_ref().unwrap()["hive"]["nextSeq"]
                            .as_u64()
                            .unwrap()
                    {
                        let update = timeout(Duration::from_secs(5), rx.recv())
                            .await
                            .unwrap()
                            .unwrap();
                        let next = update.meta.unwrap()["hive"]["seq"].as_u64().unwrap();
                        assert_eq!(
                            next,
                            seq + 1,
                            "every retained event is replayed exactly once"
                        );
                        seq = next;
                    }
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
                Ok(resumed.meta.unwrap()["hive"]["generation"]
                    .as_u64()
                    .unwrap())
            })
            .await
            .unwrap()
    };
    let (first, second) = tokio::join!(
        takeover(format!("ws://{address}"), id.clone(), barrier.clone()),
        takeover(format!("http://{address}"), id, barrier),
    );
    assert_eq!([first.min(second), first.max(second)], [2, 3]);
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
            cx.send_request(v2::InitializeRequest::new(ProtocolVersion::V2, v2::Implementation::new("pilot", "1"))).block_task().await?;
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
