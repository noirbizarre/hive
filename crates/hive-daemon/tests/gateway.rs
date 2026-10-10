use std::{collections::BTreeMap, net::SocketAddr, time::Duration};

use agent_client_protocol::{
    Agent, Client, Error, Responder, V2ConnectionTo,
    schema::{ProtocolVersion, v2},
};
use agent_client_protocol_http::HttpClient;
use hive_daemon::config::{Config, Profile};
use hive_protocol::AgentVersion;
use tokio::{sync::mpsc, time::timeout};

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
            let opened = cx.build_session_cwd()?.start_session().block_task().await?;
            let session = opened.into_session();
            let id = session.session_id().clone();
            let second = cx
                .build_session_cwd()?
                .start_session()
                .block_task()
                .await?
                .into_session();
            let other_id = second.session_id().clone();
            assert_ne!(id, other_id);
            // Publish both requests before waiting for either completion.
            let first_prompt = session.send_prompt("hello hive").block_task();
            let second_prompt = second.send_prompt("other session").block_task();
            let (response, other_response) = tokio::try_join!(first_prompt, second_prompt)?;
            assert!(!response.message_id.to_string().is_empty());
            assert!(!other_response.message_id.to_string().is_empty());
            let mut texts = [false; 2];
            let mut running = [false; 2];
            let mut idle = [false; 2];
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
                                    "Echo: hello hive"
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
                    _ => {}
                }
            }
            assert!(texts.iter().all(|seen| *seen));
            let listed = cx
                .send_request(v2::ListSessionsRequest::new())
                .block_task()
                .await?;
            assert_eq!(listed.sessions.len(), 2);
            if version == AgentVersion::V2 {
                let cwd = std::env::current_dir().map_err(Error::into_internal_error)?;
                let request = v2::ResumeSessionRequest::new(id.clone(), cwd)
                    .replay_from(v2::ReplayFrom::Start(v2::ReplayFromStart::new()));
                cx.send_request(request).block_task().await?;
                let mut replayed = false;
                while let Ok(update) = rx.try_recv() {
                    if update.session_id == id
                        && matches!(update.update, v2::SessionUpdate::AgentMessage(_))
                    {
                        replayed = true;
                    }
                }
                assert!(replayed, "resume must replay retained snapshots");
                cx.send_notification(v2::CancelSessionNotification::new(id.clone()))?;
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
            session.close().block_task().await?;
            second.send_prompt("still running").block_task().await?;
            let continued = timeout(Duration::from_secs(10), async {
                while let Some(update) = rx.recv().await {
                    if update.session_id == other_id
                        && let v2::SessionUpdate::AgentMessageChunk(chunk) = update.update
                        && let v2::ContentBlock::Text(text) = chunk.content
                        && text.text == "Echo: still running"
                    {
                        return true;
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
            second.close().block_task().await?;
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
                cx.send_request(prompt).block_task().await?;
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
                cx.send_request(v2::ResumeSessionRequest::new(id.clone(), cwd))
                    .block_task()
                    .await?;
                cx.send_request(v2::PromptRequest::new(id.clone(), vec!["after".into()]))
                    .block_task()
                    .await?;
                let saw_reply = timeout(Duration::from_secs(5), async {
                    while let Some(update) = rx.recv().await {
                        if update.session_id == id
                            && let v2::SessionUpdate::AgentMessageChunk(chunk) = update.update
                            && let v2::ContentBlock::Text(text) = chunk.content
                            && text.text == "Echo: after"
                        {
                            return true;
                        }
                    }
                    false
                })
                .await
                .map_err(Error::into_internal_error)?;
                assert!(saw_reply);
                cx.send_request(v2::CloseSessionRequest::new(id))
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
async fn a_second_daemon_cannot_start_on_another_port_for_the_same_user() {
    let runtime = std::env::temp_dir().join(format!("hive-instance-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&runtime).unwrap();
    let reserve = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = reserve.local_addr().unwrap();
    drop(reserve);
    let first = tokio::process::Command::new(env!("CARGO_BIN_EXE_hive"))
        .args(["serve", "--listen", &address.to_string()])
        .env("XDG_RUNTIME_DIR", &runtime)
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut first = first;
    let status_url = format!("http://{address}/hive/v1/status");
    timeout(Duration::from_secs(5), async {
        loop {
            if reqwest::get(&status_url).await.is_ok() {
                break;
            }
            assert!(
                first.try_wait().unwrap().is_none(),
                "first daemon exited early"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    let second = timeout(
        Duration::from_secs(5),
        tokio::process::Command::new(env!("CARGO_BIN_EXE_hive"))
            .args(["serve", "--listen", "127.0.0.1:0"])
            .env("XDG_RUNTIME_DIR", &runtime)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(!second.status.success());
    assert!(String::from_utf8_lossy(&second.stderr).contains("already running"));
    assert!(
        reqwest::get(&status_url)
            .await
            .unwrap()
            .status()
            .is_success()
    );
    let status = tokio::process::Command::new(env!("CARGO_BIN_EXE_hive"))
        .args(["status", "--url", &format!("http://{address}")])
        .output()
        .await
        .unwrap();
    assert!(status.status.success());
    assert!(String::from_utf8_lossy(&status.stdout).contains("\"sessions\":0"));
    #[cfg(unix)]
    {
        let signal = tokio::process::Command::new("kill")
            .args(["-TERM", &first.id().unwrap().to_string()])
            .status()
            .await
            .unwrap();
        assert!(signal.success());
        assert!(
            timeout(Duration::from_secs(5), first.wait())
                .await
                .unwrap()
                .unwrap()
                .success()
        );
    }
    #[cfg(not(unix))]
    first.kill().await.unwrap();
    let third = tokio::process::Command::new(env!("CARGO_BIN_EXE_hive"))
        .args(["serve", "--listen", &address.to_string()])
        .env("XDG_RUNTIME_DIR", &runtime)
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut third = third;
    timeout(Duration::from_secs(5), async {
        loop {
            if reqwest::get(&status_url).await.is_ok() {
                break;
            }
            assert!(
                third.try_wait().unwrap().is_none(),
                "daemon could not restart"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    third.kill().await.unwrap();
    std::fs::remove_dir_all(runtime).unwrap();
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
