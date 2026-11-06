//! Real owner Hive coverage: the initiating side owns a connection, not a proxy session.
use super::*;
use axum::{
    body::Body,
    extract::{Request, State},
    response::{IntoResponse, Response},
    routing::any,
};
use hive_protocol::transport::Credentials;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::watch;
use tokio_stream::StreamExt;

#[tokio::test]
async fn peer_endpoints_keep_their_exact_path_instead_of_appending_another_acp_segment() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!(
        "http://{}/private/gateway/acp",
        listener.local_addr().unwrap()
    );
    let router = axum::Router::new().nest(
        "/private/gateway",
        hive_daemon::router(hive_daemon::Broker::new(Config::default())),
    );
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    Client
        .v2()
        .connect_with(
            Credentials::default().peer(&endpoint).unwrap(),
            async |cx| {
                cx.send_request(v2::InitializeRequest::new(
                    ProtocolVersion::V2,
                    v2::Implementation::new("exact-path-test", "1"),
                ))
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
async fn nested_session_location_payloads_remain_connection_scoped_without_an_attachment() {
    use agent_client_protocol::ConnectTo;
    let (address, server) = start_gateway(Config::default()).await;
    let (mut channel, driver) = ConnectTo::<Client>::into_channel_and_future(
        Credentials::default()
            .peer(&format!("http://{address}/acp"))
            .unwrap(),
    );
    let driver = tokio::spawn(driver.unwrap());
    let initialized = raw_exchange(&mut channel, serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":2,"info":{"name":"scope-test","version":"1"},"capabilities":{}}})).await;
    assert_eq!(initialized["result"]["protocolVersion"], 2);
    let response = raw_exchange(&mut channel, serde_json::json!({"jsonrpc":"2.0","id":2,"method":"_hive/sessions/resolve","params":{"session":{"owner":"node-11111111-1111-4111-8111-111111111111","id":"hive-example"}}})).await;
    // A legacy client cannot resolve, but its error still uses the connection stream:
    // looking up a nested reference must not require an existing SSE attachment.
    assert_eq!(response["id"], 2);
    assert_eq!(response["error"]["code"], -32060);
    assert_eq!(
        response["error"]["data"]["code"],
        "hive::remote::unsupported"
    );
    assert!(
        summaries(address).await["sessions"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    driver.abort();
    server.abort();
}

#[derive(Clone)]
struct Proxy {
    owner: SocketAddr,
    stop: watch::Sender<bool>,
    new_sessions: Arc<AtomicUsize>,
    closes: Arc<AtomicUsize>,
}

async fn forward(State(proxy): State<Proxy>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let body = axum::body::to_bytes(body, 16 * 1024 * 1024).await.unwrap();
    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&body) {
        match value["method"].as_str() {
            Some("session/new") => {
                proxy.new_sessions.fetch_add(1, Ordering::SeqCst);
            }
            Some("session/close") => {
                proxy.closes.fetch_add(1, Ordering::SeqCst);
            }
            _ => {}
        }
    }
    let response = reqwest::Client::new()
        .request(parts.method, format!("http://{}/acp", proxy.owner))
        .headers(parts.headers)
        .body(body)
        .send()
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let stream = headers
        .get("content-type")
        .is_some_and(|value| value == "text/event-stream");
    let body = if stream {
        let mut bytes = response.bytes_stream();
        let mut stop = proxy.stop.subscribe();
        let (tx, rx) = mpsc::channel(8);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = stop.changed() => break,
                    next = bytes.next() => match next {
                        Some(bytes) => if tx.send(bytes).await.is_err() { break; },
                        None => break,
                    }
                }
            }
        });
        Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(rx))
    } else {
        Body::from(response.bytes().await.unwrap())
    };
    let mut response = Response::new(body);
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}

async fn idle(
    rx: &mut mpsc::UnboundedReceiver<v2::UpdateSessionNotification>,
    expected: &str,
) -> agent_client_protocol::Result<()> {
    timeout(Duration::from_secs(5), async {
        let mut reply = false;
        while let Some(update) = rx.recv().await {
            match update.update {
                v2::SessionUpdate::AgentMessageChunk(chunk) => {
                    if matches!(chunk.content, v2::ContentBlock::Text(ref text) if text.text == expected) { reply = true; }
                }
                v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(_)) if reply => return,
                _ => {}
            }
        }
        panic!("owner stream ended before the expected reply and idle");
    }).await.map_err(Error::into_internal_error)
}

#[tokio::test]
async fn peer_transport_loss_and_explicit_disconnect_preserve_the_owner_session() {
    for forced_loss in [false, true] {
        let config = config_with_agent(
            "mock",
            env!("CARGO_BIN_EXE_hive-test-agent"),
            AgentVersion::V2,
        );
        let broker = hive_daemon::Broker::new(config);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let owner = listener.local_addr().unwrap();
        let policy = hive_daemon::http::ServeOptions::default()
            .policy(owner)
            .unwrap();
        let router = hive_daemon::router_with_policy(broker.clone(), policy);
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let proxy = Proxy {
            owner,
            stop: watch::channel(false).0,
            new_sessions: Arc::default(),
            closes: Arc::default(),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/acp", listener.local_addr().unwrap());
        let router = axum::Router::new()
            .route("/acp", any(forward))
            .with_state(proxy.clone());
        let gateway = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let (created, received) = oneshot::channel();
        let transport = Credentials::default().peer(&endpoint).unwrap();
        let first = tokio::spawn(async move {
            let (tx, mut rx) = mpsc::unbounded_channel();
            Client
                .v2()
                .on_receive_notification(
                    async move |update: v2::UpdateSessionNotification, _: V2ConnectionTo<Agent>| {
                        tx.send(update).map_err(Error::into_internal_error)
                    },
                    agent_client_protocol::on_receive_notification!(),
                )
                .connect_with(transport, async move |cx| {
                    cx.send_request(v2::InitializeRequest::new(
                        ProtocolVersion::V2,
                        v2::Implementation::new("hive-peer", "1"),
                    ))
                    .block_task()
                    .await?;
                    let opened = cx
                        .send_request(v2::NewSessionRequest::new(std::env::current_dir().unwrap()))
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
                        .meta(opened.meta.clone()),
                    )
                    .block_task()
                    .await?;
                    cx.send_request(
                        v2::PromptRequest::new(opened.session_id.clone(), vec!["before".into()])
                            .meta(controlled(&opened.meta, "before")),
                    )
                    .block_task()
                    .await?;
                    idle(&mut rx, "Echo: before").await?;
                    created.send(opened.session_id).unwrap();
                    if forced_loss {
                        std::future::pending::<()>().await;
                    }
                    Ok(())
                })
                .await
        });
        let id = timeout(Duration::from_secs(15), received)
            .await
            .unwrap()
            .unwrap();
        if forced_loss {
            proxy.stop.send(true).unwrap();
        }
        let result = timeout(Duration::from_secs(5), first)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.is_err(), forced_loss);
        assert_eq!(proxy.closes.load(Ordering::SeqCst), 0);
        assert_eq!(
            summaries(owner).await["sessions"].as_array().unwrap().len(),
            1
        );
        // Wait for attachment cleanup before explicitly acquiring a new controller generation.
        timeout(Duration::from_secs(5), async {
            loop {
                if summaries(owner).await["sessions"][0]["controlOwner"].is_null() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        timeout(Duration::from_secs(15), Client.v2().on_receive_notification(
            async move |update: v2::UpdateSessionNotification, _: V2ConnectionTo<Agent>| { tx.send(update).map_err(Error::into_internal_error) },
            agent_client_protocol::on_receive_notification!(),
        ).connect_with(Credentials::default().peer(&endpoint).unwrap(), async |cx| {
            cx.send_request(v2::InitializeRequest::new(ProtocolVersion::V2, v2::Implementation::new("hive-peer-reconnected", "1"))).block_task().await?;
            let resumed = cx.send_request(v2::ResumeSessionRequest::new(id.clone(), std::env::current_dir().unwrap())).block_task().await?;
            assert_eq!(serde_json::to_value(&resumed.config_options).unwrap()[0]["currentValue"], "careful");
            cx.send_request(v2::PromptRequest::new(id.clone(), vec!["after".into()]).meta(controlled(&resumed.meta, "after"))).block_task().await?;
            idle(&mut rx, "Echo: after").await?;
            cx.send_notification(v2::CancelSessionNotification::new(id.clone()).meta(resumed.meta.clone()))?;
            cx.send_request(v2::CloseSessionRequest::new(id.clone()).meta(resumed.meta)).block_task().await?;
            Ok(())
        })).await.unwrap().unwrap();
        assert_eq!(proxy.new_sessions.load(Ordering::SeqCst), 1);
        assert_eq!(proxy.closes.load(Ordering::SeqCst), 1);
        broker.shutdown().await;
        gateway.abort();
        server.abort();
    }
}

#[tokio::test]
async fn malformed_http_requests_are_rejected_without_allocating_sessions() {
    let (address, server) = start_gateway(Config::default()).await;
    let http = reqwest::Client::new();
    let endpoint = format!("http://{address}/acp");
    assert_eq!(
        http.post(&endpoint)
            .body("{}")
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::UNSUPPORTED_MEDIA_TYPE
    );
    for body in [
        "{",
        "{}",
        "[]",
        r#"{"jsonrpc":"2.0","id":1,"method":"session/new","params":{}}"#,
    ] {
        assert_eq!(
            http.post(&endpoint)
                .header("content-type", "application/json")
                .body(body)
                .send()
                .await
                .unwrap()
                .status(),
            reqwest::StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        http.post(&endpoint)
            .header("Acp-Connection-Id", "unknown")
            .json(&serde_json::json!({"jsonrpc":"2.0","method":"session/new","id":1,"params":{}}))
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::NOT_FOUND
    );
    let initialized = http.post(&endpoint).json(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":2,"info":{"name":"malformed-test","version":"1"},"capabilities":{}}})).send().await.unwrap();
    let connection = initialized.headers()["Acp-Connection-Id"].clone();
    assert_eq!(http.post(&endpoint).header("Acp-Connection-Id", connection.clone()).header("Acp-Session-Id", "one").json(&serde_json::json!({"jsonrpc":"2.0","id":2,"method":"session/prompt","params":{"sessionId":"two","prompt":[]}})).send().await.unwrap().status(), reqwest::StatusCode::BAD_REQUEST);
    let stream = http
        .get(&endpoint)
        .header("Acp-Connection-Id", connection.clone())
        .header("accept", "text/event-stream")
        .send()
        .await
        .unwrap();
    let mut stream = stream.bytes_stream();
    // Once initialized, malformed JSON is a JSON-RPC parse error on the connection SSE stream.
    assert_eq!(
        http.post(&endpoint)
            .header("Acp-Connection-Id", connection.clone())
            .header("content-type", "application/json")
            .body("{")
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::ACCEPTED
    );
    timeout(Duration::from_secs(5), async {
        let mut received = String::new();
        while let Some(chunk) = stream.next().await {
            received.push_str(&String::from_utf8_lossy(&chunk.unwrap()));
            if received.contains("-32700") {
                return;
            }
        }
        panic!("malformed frame did not produce a parse error");
    })
    .await
    .unwrap();
    http.delete(&endpoint)
        .header("Acp-Connection-Id", connection)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert!(
        summaries(address).await["sessions"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    server.abort();
}

#[tokio::test]
async fn peer_connections_fail_on_server_errors_and_never_follow_redirects() {
    for status in [302, 400, 500, 503] {
        let hits = Arc::new(AtomicUsize::new(0));
        let target_hits = hits.clone();
        let router = axum::Router::new()
            .route(
                "/target",
                any(move || async move {
                    target_hits.fetch_add(1, Ordering::SeqCst);
                    "unexpected"
                }),
            )
            .route(
                "/acp",
                any(move || async move {
                    (
                        axum::http::StatusCode::from_u16(status).unwrap(),
                        [("location", "/target")],
                        "untrusted-server-secret",
                    )
                        .into_response()
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/acp", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let result = timeout(
            Duration::from_secs(5),
            Client.v2().connect_with(
                Credentials::bearer("private-client-token")
                    .unwrap()
                    .peer(&endpoint)
                    .unwrap(),
                async |cx| {
                    cx.send_request(v2::InitializeRequest::new(
                        ProtocolVersion::V2,
                        v2::Implementation::new("failure-test", "1"),
                    ))
                    .block_task()
                    .await?;
                    Ok(())
                },
            ),
        )
        .await
        .unwrap();
        let error = result.unwrap_err();
        let diagnostic = serde_json::to_string(&error).unwrap();
        assert!(diagnostic.contains("hive::transport::connection"));
        assert!(!diagnostic.contains("untrusted-server-secret"));
        assert!(!diagnostic.contains("private-client-token"));
        assert_eq!(hits.load(Ordering::SeqCst), 0);
        server.abort();
    }
}

#[tokio::test]
async fn malformed_initialization_and_failed_sse_establishment_fail_the_peer_connection() {
    for malformed in [true, false] {
        let router = axum::Router::new().route("/acp", axum::routing::post(move |axum::Json(request): axum::Json<serde_json::Value>| async move {
            if malformed { return ([("Acp-Connection-Id", "fixture")], "not JSON-RPC").into_response(); }
            ([("Acp-Connection-Id", "fixture")], axum::Json(serde_json::json!({"jsonrpc":"2.0","id":request["id"],"result":{"protocolVersion":2,"info":{"name":"fixture","version":"1"},"capabilities":{}}}))).into_response()
        }).get(|| async { axum::http::StatusCode::SERVICE_UNAVAILABLE }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/acp", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let result = timeout(
            Duration::from_secs(5),
            Client.v2().connect_with(
                Credentials::default().peer(&endpoint).unwrap(),
                async |cx| {
                    cx.send_request(v2::InitializeRequest::new(
                        ProtocolVersion::V2,
                        v2::Implementation::new("failure-test", "1"),
                    ))
                    .block_task()
                    .await?;
                    // Keep the connection alive so an immediate SSE failure cannot race successful closure.
                    std::future::pending::<agent_client_protocol::Result<()>>().await
                },
            ),
        )
        .await
        .unwrap();
        assert!(result.is_err());
        server.abort();
    }
}
