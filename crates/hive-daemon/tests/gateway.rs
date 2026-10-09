use std::{net::SocketAddr, time::Duration};

use agent_client_protocol::{
    Agent, Client, Error, V2ConnectionTo,
    schema::{ProtocolVersion, v2},
};
use agent_client_protocol_http::HttpClient;
use hive_protocol::{AgentVersion, StdioProvider};
use tokio::{sync::mpsc, time::timeout};

async fn exercise_gateway(transport: &str, version: AgentVersion) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address: SocketAddr = listener.local_addr().unwrap();
    let command = if version == AgentVersion::V1 {
        format!("{} v1", env!("CARGO_BIN_EXE_hive-test-agent"))
    } else {
        env!("CARGO_BIN_EXE_hive-test-agent").to_string()
    };
    let broker = hive_daemon::Broker::new(StdioProvider::new(command, version));
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
    assert_eq!(
        provider["protocolVersion"],
        if version == AgentVersion::V1 { 1 } else { 2 }
    );
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
    let provider = StdioProvider::new("unused", AgentVersion::V2);
    let error = hive_daemon::serve("0.0.0.0:0".parse().unwrap(), provider)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
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
