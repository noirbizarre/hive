//! Controlled owner response barriers expose races that successful round trips cannot exercise.
use super::*;
use agent_client_protocol::UntypedMessage;
use hive_protocol::remote::{ListPeers, Peer};
use serde_json::json;
use std::time::Duration;

struct Held {
    request: UntypedMessage,
    responder: Responder<Value>,
    connection: V2ConnectionTo<Client>,
}

struct Fixture {
    broker: Broker,
    owner: NodeId,
    address: SocketAddr,
    held: mpsc::UnboundedReceiver<Held>,
    notifications: mpsc::UnboundedReceiver<UntypedMessage>,
    servers: Vec<tokio::task::JoinHandle<()>>,
    _directory: tempfile::TempDir,
}

async fn serve(router: Router) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    (
        address,
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        }),
    )
}

fn metadata(owner: &NodeId) -> Value {
    json!({"session":{"owner":owner,"id":"retained"},"generation":1,"token":"owner-private-token","role":"pilot"})
}

impl Fixture {
    async fn new(target: &'static str) -> Self {
        let owner = NodeId::new();
        let node = owner.clone();
        let (questions, held) = mpsc::unbounded_channel();
        let (events, notifications) = mpsc::unbounded_channel();
        let router = AcpHttpServer::new(move || {
            let owner = node.clone(); let questions = questions.clone(); let events = events.clone();
            Agent.v2().on_receive_request(async move |request: UntypedMessage, responder: Responder<Value>, cx: V2ConnectionTo<Client>| {
                if request.method == target && (target != "_hive/sessions/resolve" || request.params["session"]["id"] == "slow") {
                    let _ = questions.send(Held { request, responder, connection: cx });
                    return Ok(());
                }
                let result = match request.method.as_str() {
                    "initialize" => json!({"protocolVersion":2,"info":{"name":"scripted-owner","version":"1"},"capabilities":{"session":{},"_meta":{"hive":hive_protocol::remote::advertisement(owner.as_str())}}}),
                    "session/new" => json!({"sessionId":"retained","_meta":{"hive":metadata(&owner)}}),
                    "session/list" => json!({"sessions":[{"sessionId":"retained","cwd":"C:\\owner\\project","_meta":{"hive":{"session":{"owner":owner,"id":"retained"}}}}]}),
                    "_hive/sessions/resolve" => json!({"session":request.params["session"],"sessionId":request.params["session"]["id"],"cwd":"C:\\owner\\project"}),
                    "session/resume" => json!({"_meta":{"hive":metadata(&owner)}}),
                    "session/prompt" => json!({"messageId":"receipt"}),
                    "session/close" => json!({}),
                    _ => return responder.respond_with_error(Error::method_not_found()),
                };
                responder.respond(result)
            }, agent_client_protocol::on_receive_request!())
            .on_receive_notification(async move |notification: UntypedMessage, _: V2ConnectionTo<Client>| {
                let _ = events.send(notification); Ok(())
            }, agent_client_protocol::on_receive_notification!())
        }).into_router();
        let (owner_address, owner_server) = serve(router).await;
        let directory = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.peers.insert(
            "owner".into(),
            config::PeerRegistration {
                owner: owner.clone(),
                endpoint: format!("http://{owner_address}/acp"),
                name: None,
                auth_token_env: None,
            },
        );
        let broker =
            Broker::persistent(config, directory.path().join("state/catalog.sqlite3")).unwrap();
        let (address, gateway_server) = serve(crate::router(broker.clone())).await;
        Self {
            broker,
            owner,
            address,
            held,
            notifications,
            servers: vec![owner_server, gateway_server],
            _directory: directory,
        }
    }

    fn transport(&self) -> HttpClient {
        HttpClient::new(format!("ws://{}", self.address)).unwrap()
    }

    async fn stop(self) {
        self.broker.shutdown().await;
        for server in self.servers {
            server.abort();
        }
    }

    fn links(&self) -> Vec<Arc<peers::Link>> {
        self.broker
            .peer_links
            .lock()
            .unwrap()
            .iter()
            .filter_map(std::sync::Weak::upgrade)
            .collect()
    }
}

async fn initialize(cx: V2ConnectionTo<Agent>) -> Result<Peer, Error> {
    Peer::initialize(
        cx,
        v2::InitializeRequest::new(
            ProtocolVersion::V2,
            v2::Implementation::new("routing-regression", "1"),
        ),
    )
    .await
}

#[tokio::test]
async fn aborting_the_owner_driver_answers_every_pending_mutation_without_terminating_upstream() {
    for method in [
        "session/new",
        "session/prompt",
        "session/set_config_option",
        "session/close",
    ] {
        let mut fixture = Fixture::new(method).await;
        tokio::time::timeout(Duration::from_secs(10), Client.v2().connect_with(fixture.transport(), async |cx| {
            let peer = initialize(cx.clone()).await?;
            let mut params = json!({"cwd":"C:\\owner\\project","_meta":{"hive":{"peer":fixture.owner,"agent":"coder"}}});
            if method != "session/new" {
                let opened = peer.start_at(&fixture.owner, "coder", v2::NewSessionRequest::new("C:\\owner\\project")).await?;
                params = json!({"sessionId":opened.session_id,"prompt":[{"type":"text","text":"once"}],"configId":"model","type":"id","value":"fast","_meta":opened.meta});
                params["_meta"]["hive"]["key"] = "one-intent".into();
            }
            let pending = cx.prepare_request(UntypedMessage::new(method, params)?).block_task();
            let held = fixture.held.recv().await.unwrap();
            assert_eq!(held.request.method, method);
            for link in fixture.links() { link.disconnect(); }
            let error = tokio::time::timeout(Duration::from_secs(2), pending).await.unwrap().unwrap_err();
            let expected = if method == "session/new" { RemoteError::LaunchUncertain } else { RemoteError::OwnerUnavailable };
            assert_eq!(error.data.unwrap()["code"], expected.code());
            cx.prepare_request(ListPeers(json!({}))).block_task().await?;
            Ok(())
        })).await.unwrap().unwrap();
        fixture.stop().await;
    }
}

#[tokio::test]
async fn discovering_a_session_before_its_creation_response_preserves_one_alias_for_early_events_and_control()
 {
    let mut fixture = Fixture::new("session/new").await;
    let (events, mut updates) = mpsc::unbounded_channel();
    tokio::time::timeout(Duration::from_secs(10), Client.v2()
        .on_receive_notification(async move |update: v2::UpdateSessionNotification, _: V2ConnectionTo<Agent>| { let _ = events.send(update); Ok(()) }, agent_client_protocol::on_receive_notification!())
        .connect_with(fixture.transport(), async |cx| {
            initialize(cx.clone()).await?;
            let request = hive_protocol::remote::NewSession(json!({"cwd":"C:\\owner\\project","_meta":{"hive":{"peer":fixture.owner,"agent":"coder"}}}));
            let pending = cx.prepare_request(request).block_task();
            let held = fixture.held.recv().await.unwrap();
            let listed = cx.prepare_request(v2::ListSessionsRequest::new()).block_task().await?;
            let alias = listed.sessions[0].session_id.clone();
            held.connection.send_notification(v2::UpdateSessionNotification::new(v2::SessionId::new("retained"), v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(v2::IdleStateUpdate::new()))))?;
            held.responder.respond(json!({"sessionId":"retained","_meta":{"hive":metadata(&fixture.owner)}}))?;
            let opened = pending.await?;
            assert_eq!(opened.session_id, alias);
            // HTTP subscribes to the session stream after new succeeds, then drains its queued events.
            assert_eq!(updates.recv().await.unwrap().session_id, alias);
            let mut meta = opened.meta.unwrap(); meta.get_mut("hive").unwrap()["key"] = "canonical-intent".into();
            cx.prepare_request(v2::PromptRequest::new(alias, vec!["once".into()]).meta(meta)).block_task().await?;
            Ok(())
        })).await.unwrap().unwrap();
    fixture.stop().await;
}

#[tokio::test]
async fn a_stalled_resume_does_not_delay_cancellation_of_another_attached_session() {
    let mut fixture = Fixture::new("_hive/sessions/resolve").await;
    let route = fixture
        .broker
        .routes
        .remember(
            &fixture.broker,
            SessionLocation {
                session: SessionReference {
                    owner: fixture.owner.clone(),
                    id: "slow".into(),
                },
                session_id: "slow".into(),
                cwd: v2::AbsolutePath::new("C:\\owner\\project"),
            },
            None,
        )
        .unwrap();
    tokio::time::timeout(
        Duration::from_secs(10),
        Client.v2().connect_with(fixture.transport(), async |cx| {
            let peer = initialize(cx.clone()).await?;
            let opened = peer
                .start_at(
                    &fixture.owner,
                    "coder",
                    v2::NewSessionRequest::new("C:\\owner\\project"),
                )
                .await?;
            let pending = cx
                .prepare_request(v2::ResumeSessionRequest::new(
                    v2::SessionId::new(route.alias.clone()),
                    route.cwd.clone(),
                ))
                .block_task();
            let held = fixture.held.recv().await.unwrap();
            cx.send_notification(
                v2::CancelSessionNotification::new(opened.session_id).meta(opened.meta),
            )?;
            let notification =
                tokio::time::timeout(Duration::from_secs(2), fixture.notifications.recv())
                    .await
                    .unwrap()
                    .unwrap();
            assert_eq!(notification.method, "session/cancel");
            assert_eq!(notification.params["sessionId"], "retained");
            // Release the unrelated lookup only after cancellation has demonstrably crossed the other hop.
            held.responder
                .respond_with_error(RemoteError::SessionNotFound.wire())?;
            assert!(pending.await.is_err());
            Ok(())
        }),
    )
    .await
    .unwrap()
    .unwrap();
    fixture.stop().await;
}

#[tokio::test]
async fn confirmed_close_and_failed_initial_resume_release_their_dedicated_owner_links() {
    let mut fixture = Fixture::new("session/resume").await;
    let route = fixture
        .broker
        .routes
        .remember(
            &fixture.broker,
            SessionLocation {
                session: SessionReference {
                    owner: fixture.owner.clone(),
                    id: "retained".into(),
                },
                session_id: "retained".into(),
                cwd: v2::AbsolutePath::new("C:\\owner\\project"),
            },
            None,
        )
        .unwrap();
    tokio::time::timeout(
        Duration::from_secs(10),
        Client.v2().connect_with(fixture.transport(), async |cx| {
            let peer = initialize(cx.clone()).await?;
            let pending = cx
                .prepare_request(v2::ResumeSessionRequest::new(
                    v2::SessionId::new(route.alias.clone()),
                    route.cwd.clone(),
                ))
                .block_task();
            fixture
                .held
                .recv()
                .await
                .unwrap()
                .responder
                .respond_with_error(Error::invalid_params())?;
            assert!(pending.await.is_err());
            for _ in 0..3 {
                let opened = peer
                    .start_at(
                        &fixture.owner,
                        "coder",
                        v2::NewSessionRequest::new("C:\\owner\\project"),
                    )
                    .await?;
                cx.prepare_request(
                    v2::CloseSessionRequest::new(opened.session_id).meta(opened.meta),
                )
                .block_task()
                .await?;
                tokio::time::timeout(Duration::from_secs(2), async {
                    while !fixture.links().is_empty() {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
            }
            assert!(!fixture.broker.routes.get(&route.alias).unwrap().present);
            Ok(())
        }),
    )
    .await
    .unwrap()
    .unwrap();
    let stored = Routes::load(fixture.broker.store.as_ref().unwrap()).unwrap();
    assert!(!stored.get(&route.alias).unwrap().present);
    fixture.stop().await;
}

#[test]
fn authoritative_absence_is_durable_without_revoking_other_scopes_or_newer_observations() {
    let directory = tempfile::tempdir().unwrap();
    let broker = Broker::persistent(
        Config::default(),
        directory.path().join("state/catalog.sqlite3"),
    )
    .unwrap();
    let owner = NodeId::new();
    let location = |id: &str, cwd: &str| SessionLocation {
        session: SessionReference {
            owner: owner.clone(),
            id: id.into(),
        },
        session_id: id.into(),
        cwd: v2::AbsolutePath::new(cwd),
    };
    let absent = broker
        .routes
        .remember(&broker, location("absent", "C:\\owner\\one"), None)
        .unwrap();
    let refreshed = broker
        .routes
        .remember(&broker, location("refreshed", "C:\\owner\\one"), None)
        .unwrap();
    let other = broker
        .routes
        .remember(&broker, location("other", "/srv/other"), None)
        .unwrap();
    let baseline = vec![absent.clone(), refreshed.clone()];
    broker
        .routes
        .remember(&broker, location("refreshed", "C:\\owner\\one"), None)
        .unwrap();
    broker
        .routes
        .reconcile(&broker, &baseline, &Default::default())
        .unwrap();
    let restored = Routes::load(broker.store.as_ref().unwrap()).unwrap();
    assert!(!restored.get(&absent.alias).unwrap().present);
    assert!(restored.get(&refreshed.alias).unwrap().present);
    assert!(restored.get(&other.alias).unwrap().present);
}
