//! Scripted peers deliberately ask before prompt acknowledgement to expose dispatch deadlocks.
use super::*;
use serde_json::json;

async fn fixture(
    mode: &'static str,
) -> (
    SocketAddr,
    tokio::task::JoinHandle<()>,
    tokio::task::JoinHandle<()>,
    mpsc::UnboundedReceiver<serde_json::Value>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let downstream = listener.local_addr().unwrap();
    let (report, reports) = mpsc::unbounded_channel();
    let agent = tokio::spawn(async move {
        axum::serve(listener, agent_client_protocol_http::AcpHttpServer::new(move || {
            let report = report.clone();
            Agent.v2()
                .on_receive_request(async |request: v2::InitializeRequest, responder: Responder<v2::InitializeResponse>, _: V2ConnectionTo<Client>| {
                    responder.respond(v2::InitializeResponse::new(request.protocol_version, v2::Implementation::new("interactions", "1")).capabilities(v2::AgentCapabilities::new().session(v2::SessionCapabilities::new())))
                }, agent_client_protocol::on_receive_request!())
                .on_receive_request(async |_: v2::NewSessionRequest, responder: Responder<v2::NewSessionResponse>, _: V2ConnectionTo<Client>| {
                    responder.respond(v2::NewSessionResponse::new(v2::SessionId::new("native")))
                }, agent_client_protocol::on_receive_request!())
                .on_receive_request(async move |request: v2::PromptRequest, responder: Responder<v2::PromptResponse>, cx: V2ConnectionTo<Client>| {
                    let report = report.clone();
                    let peer = cx.clone();
                    cx.spawn(async move {
                        let permission = |title| v2::RequestPermissionRequest::new(request.session_id.clone(), title,
                             vec![v2::PermissionOption::new("allow", "Allow once", v2::PermissionOptionKind::AllowOnce)])
                            .description("Review this operation")
                            .subject(serde_json::from_value::<v2::RequestPermissionSubject>(json!({"type":"future_operation","context":{"preserved":true}})).unwrap())
                            .meta(v2::Meta::from_iter([("future".into(), json!({"opaque":[1,2,3]}))]));
                        let result = match mode {
                            "concurrent" => {
                                let (first, second) = tokio::join!(peer.send_request(permission("first")).block_task(), peer.send_request(permission("second")).block_task());
                                json!([first.unwrap(), second.unwrap()])
                            }
                            "cancel" => json!(peer.send_request(permission("cancel me")).block_task().await.unwrap()),
                            "request" | "unsupported" => {
                                let scope = if mode == "request" { v2::ElicitationRequestScope::new(responder.id().clone()).into() }
                                    else { v2::ElicitationScope::from(v2::ElicitationSessionScope::new(request.session_id.clone())) };
                                let schema = serde_json::from_value(json!({"type":"object","properties":{"approved":{"type":"boolean"}},"required":["approved"]})).unwrap();
                                json!(peer.send_request(v2::CreateElicitationRequest::new(v2::ElicitationFormMode::new(scope, schema), "Approve?")).block_task().await.unwrap())
                            }
                            "url-early" | "url-late" => {
                                let pending = peer.send_request(v2::CreateElicitationRequest::new(v2::ElicitationUrlMode::new(v2::ElicitationSessionScope::new(request.session_id.clone()), "shared-id", "https://example.com/auth"), "Authenticate")).block_task();
                                let completion: v2::CompleteElicitationNotification = serde_json::from_value(json!({"elicitationId":"shared-id","_meta":{"future":{"keep":true}}})).unwrap();
                                if mode == "url-early" { peer.send_notification(completion.clone())?; }
                                let response = pending.await?;
                                peer.send_notification(completion.clone())?;
                                peer.send_notification(completion)?;
                                json!(response)
                            }
                            _ => unreachable!(),
                        };
                        report.send(result).unwrap();
                        responder.respond(v2::PromptResponse::new(v2::MessageId::new("ack")))
                    })
                }, agent_client_protocol::on_receive_request!())
        }).into_router()).await.unwrap();
    });
    let mut config = Config {
        default_agent: Some("interactions".into()),
        ..Config::default()
    };
    let profile: Profile = serde_json::from_value(
        json!({"endpoint":format!("http://{downstream}"),"versions":["v2"]}),
    )
    .unwrap();
    config.agents.insert("interactions".into(), profile);
    let (address, gateway) = start_gateway(config).await;
    (address, gateway, agent, reports)
}

async fn initialize(
    cx: &V2ConnectionTo<Agent>,
    forms: bool,
) -> agent_client_protocol::Result<v2::NewSessionResponse> {
    let mut capabilities = v2::ClientCapabilities::new();
    if forms {
        capabilities = capabilities.elicitation(
            v2::ElicitationCapabilities::new()
                .form(v2::ElicitationFormCapabilities::new())
                .url(v2::ElicitationUrlCapabilities::new()),
        );
    }
    cx.send_request(
        v2::InitializeRequest::new(ProtocolVersion::V2, v2::Implementation::new("pilot", "1"))
            .capabilities(capabilities),
    )
    .block_task()
    .await?;
    cx.send_request(v2::NewSessionRequest::new(std::env::current_dir().unwrap()))
        .block_task()
        .await
}

#[tokio::test]
async fn concurrent_permissions_before_prompt_acknowledgement_keep_their_own_responses() {
    let (address, gateway, agent, mut reports) = fixture("concurrent").await;
    let (tx, mut rx) = mpsc::unbounded_channel();
    timeout(
        Duration::from_secs(10),
        Client
            .v2()
            .on_receive_request(
                async move |request: v2::RequestPermissionRequest,
                            responder: Responder<v2::RequestPermissionResponse>,
                            _: V2ConnectionTo<Agent>| {
                    tx.send((request, responder)).unwrap();
                    Ok(())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(
                HttpClient::new(format!("ws://{address}")).unwrap(),
                async move |cx| {
                    let opened = initialize(&cx, false).await?;
                    let pending = cx
                        .send_request(
                            v2::PromptRequest::new(opened.session_id.clone(), vec!["go".into()])
                                .meta(controlled(&opened.meta, "concurrent")),
                        )
                        .block_task();
                    let (first, first_response) = rx.recv().await.unwrap();
                    let (second, second_response) = rx.recv().await.unwrap();
                    assert_eq!(first.session_id, opened.session_id);
                    assert_eq!(
                        serde_json::to_value(first.subject).unwrap(),
                        json!({"type":"future_operation","context":{"preserved":true}})
                    );
                    assert_eq!(first.meta.unwrap()["future"]["opaque"], json!([1, 2, 3]));
                    assert_eq!(first.description.as_deref(), Some("Review this operation"));
                    assert_ne!(first.title, second.title);
                    second_response.respond(v2::RequestPermissionResponse::new(
                        v2::RequestPermissionOutcome::Selected(v2::SelectedPermissionOutcome::new(
                            "allow",
                        )),
                    ))?;
                    first_response.respond(v2::RequestPermissionResponse::new(
                        v2::RequestPermissionOutcome::Cancelled,
                    ))?;
                    pending.await?;
                    let results = reports.recv().await.unwrap();
                    assert_eq!(results[0]["outcome"]["outcome"], "cancelled");
                    assert_eq!(results[1]["outcome"]["outcome"], "selected");
                    Ok(())
                },
            ),
    )
    .await
    .unwrap()
    .unwrap();
    gateway.abort();
    agent.abort();
}

#[tokio::test]
async fn session_cancellation_withdraws_a_pending_permission_without_waiting_for_the_user() {
    let (address, gateway, agent, mut reports) = fixture("cancel").await;
    let (tx, mut rx) = mpsc::unbounded_channel();
    timeout(
        Duration::from_secs(10),
        Client
            .v2()
            .on_receive_request(
                async move |_: v2::RequestPermissionRequest,
                            responder: Responder<v2::RequestPermissionResponse>,
                            _: V2ConnectionTo<Agent>| {
                    tx.send(responder).unwrap();
                    Ok(())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(
                HttpClient::new(format!("ws://{address}")).unwrap(),
                async move |cx| {
                    let opened = initialize(&cx, false).await?;
                    let pending = cx
                        .send_request(
                            v2::PromptRequest::new(opened.session_id.clone(), vec!["go".into()])
                                .meta(controlled(&opened.meta, "cancel")),
                        )
                        .block_task();
                    let responder = rx.recv().await.unwrap();
                    let cancellation = responder.cancellation();
                    cx.send_notification(
                        v2::CancelSessionNotification::new(opened.session_id)
                            .meta(opened.meta.unwrap()),
                    )?;
                    cancellation.cancelled().await;
                    let _ = responder.respond_with_error(Error::request_cancelled());
                    pending.await?;
                    assert_eq!(
                        reports.recv().await.unwrap()["outcome"]["outcome"],
                        "cancelled"
                    );
                    Ok(())
                },
            ),
    )
    .await
    .unwrap()
    .unwrap();
    gateway.abort();
    agent.abort();
}

#[tokio::test]
async fn request_scoped_forms_reference_the_original_live_client_rpc() {
    let (address, gateway, agent, mut reports) = fixture("request").await;
    let (tx, mut rx) = mpsc::unbounded_channel();
    timeout(Duration::from_secs(10), Client.v2()
        .on_receive_request(async move |request: v2::CreateElicitationRequest, responder: Responder<v2::CreateElicitationResponse>, _: V2ConnectionTo<Agent>| {
            tx.send((request, responder)).unwrap(); Ok(())
        }, agent_client_protocol::on_receive_request!())
        .connect_with(HttpClient::new(format!("ws://{address}")).unwrap(), async move |cx| {
            let opened = initialize(&cx, true).await?;
            let prepared = cx.prepare_request(v2::PromptRequest::new(opened.session_id, vec!["go".into()]).meta(controlled(&opened.meta, "request")));
            let id = prepared.id().clone();
            let pending = prepared.block_task();
            let (request, responder) = rx.recv().await.unwrap();
            assert!(matches!(request.scope(), v2::ElicitationScope::Request(scope) if scope.request_id == id));
            responder.respond(serde_json::from_value(json!({"action":"accept","content":{"approved":false}})).unwrap())?;
            pending.await?;
            assert_eq!(reports.recv().await.unwrap()["content"]["approved"], false);
            Ok(())
        })).await.unwrap().unwrap();
    gateway.abort();
    agent.abort();
}

#[tokio::test]
async fn missing_elicitation_capabilities_cancel_forms_instead_of_sending_unrenderable_requests() {
    let (address, gateway, agent, mut reports) = fixture("unsupported").await;
    let (tx, mut rx) = mpsc::unbounded_channel();
    timeout(
        Duration::from_secs(10),
        Client
            .v2()
            .on_receive_request(
                async move |_: v2::CreateElicitationRequest,
                            responder: Responder<v2::CreateElicitationResponse>,
                            _: V2ConnectionTo<Agent>| {
                    tx.send(()).unwrap();
                    responder.respond(v2::CreateElicitationResponse::new(
                        v2::ElicitationAction::Cancel,
                    ))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(
                HttpClient::new(format!("ws://{address}")).unwrap(),
                async move |cx| {
                    let opened = initialize(&cx, false).await?;
                    cx.send_request(
                        v2::PromptRequest::new(opened.session_id, vec!["go".into()])
                            .meta(controlled(&opened.meta, "unsupported")),
                    )
                    .block_task()
                    .await?;
                    assert_eq!(reports.recv().await.unwrap()["action"], "cancel");
                    assert!(
                        rx.try_recv().is_err(),
                        "an unsupported form must not be delivered"
                    );
                    Ok(())
                },
            ),
    )
    .await
    .unwrap()
    .unwrap();
    gateway.abort();
    agent.abort();
}

#[tokio::test]
async fn url_completion_is_correlated_before_or_after_acceptance_and_duplicates_are_ignored() {
    for mode in ["url-early", "url-late"] {
        let (address, gateway, agent, mut reports) = fixture(mode).await;
        let (tx, mut requests) = mpsc::unbounded_channel();
        let (done, mut completions) = mpsc::unbounded_channel();
        timeout(
            Duration::from_secs(10),
            Client
                .v2()
                .on_receive_request(
                    async move |request: v2::CreateElicitationRequest,
                                responder: Responder<v2::CreateElicitationResponse>,
                                _: V2ConnectionTo<Agent>| {
                        tx.send((request, responder)).unwrap();
                        Ok(())
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_notification(
                    async move |notification: v2::CompleteElicitationNotification,
                                _: V2ConnectionTo<Agent>| {
                        done.send(notification).unwrap();
                        Ok(())
                    },
                    agent_client_protocol::on_receive_notification!(),
                )
                .connect_with(
                    HttpClient::new(format!("ws://{address}")).unwrap(),
                    async move |cx| {
                        let opened = initialize(&cx, true).await?;
                        let pending = cx
                            .send_request(
                                v2::PromptRequest::new(opened.session_id, vec!["go".into()])
                                    .meta(controlled(&opened.meta, mode)),
                            )
                            .block_task();
                        let (request, responder) = timeout(Duration::from_secs(3), requests.recv())
                            .await
                            .expect("URL create was not forwarded")
                            .unwrap();
                        let v2::ElicitationMode::Url(url) = request.mode else {
                            panic!("expected URL")
                        };
                        assert_ne!(url.elicitation_id.to_string(), "shared-id");
                        let completed = if mode == "url-early" {
                            Some(
                                timeout(Duration::from_secs(3), completions.recv())
                                    .await
                                    .expect("early completion was not forwarded")
                                    .unwrap(),
                            )
                        } else {
                            None
                        };
                        responder.respond(v2::CreateElicitationResponse::new(
                            v2::ElicitationAction::Accept(v2::ElicitationAcceptAction::new()),
                        ))?;
                        let completed = match completed {
                            Some(completed) => completed,
                            None => completions.recv().await.unwrap(),
                        };
                        assert_eq!(completed.elicitation_id, url.elicitation_id);
                        assert_eq!(completed.meta.unwrap()["future"]["keep"], true);
                        pending.await?;
                        assert_eq!(reports.recv().await.unwrap()["action"], "accept");
                        assert!(completions.try_recv().is_err());
                        Ok(())
                    },
                ),
        )
        .await
        .unwrap()
        .unwrap();
        gateway.abort();
        agent.abort();
    }
}

#[tokio::test]
async fn equal_url_ids_from_two_workers_remain_distinct_on_one_client_connection() {
    let (address, gateway, agent, _reports) = fixture("url-late").await;
    let (tx, mut requests) = mpsc::unbounded_channel();
    let (done, mut completions) = mpsc::unbounded_channel();
    timeout(Duration::from_secs(10), Client.v2()
        .on_receive_request(async move |request: v2::CreateElicitationRequest, responder: Responder<v2::CreateElicitationResponse>, _: V2ConnectionTo<Agent>| { tx.send((request, responder)).unwrap(); Ok(()) }, agent_client_protocol::on_receive_request!())
        .on_receive_notification(async move |notification: v2::CompleteElicitationNotification, _: V2ConnectionTo<Agent>| { done.send(notification).unwrap(); Ok(()) }, agent_client_protocol::on_receive_notification!())
        .connect_with(HttpClient::new(format!("ws://{address}")).unwrap(), async move |cx| {
            let first = initialize(&cx, true).await?;
            let second = cx.send_request(v2::NewSessionRequest::new(std::env::current_dir().unwrap())).block_task().await?;
            let first_ack = cx.send_request(v2::PromptRequest::new(first.session_id.clone(), vec!["go".into()]).meta(controlled(&first.meta, "first"))).block_task();
            let second_ack = cx.send_request(v2::PromptRequest::new(second.session_id.clone(), vec!["go".into()]).meta(controlled(&second.meta, "second"))).block_task();
            let (first_request, first_response) = requests.recv().await.unwrap();
            let (second_request, second_response) = requests.recv().await.unwrap();
            let (v2::ElicitationMode::Url(first_url), v2::ElicitationMode::Url(second_url)) = (first_request.mode, second_request.mode) else { panic!("URLs expected") };
            assert_ne!(first_url.elicitation_id, second_url.elicitation_id);
            assert_ne!(first_url.scope, second_url.scope);
            let accept = || v2::CreateElicitationResponse::new(v2::ElicitationAction::Accept(v2::ElicitationAcceptAction::new()));
            second_response.respond(accept())?;
            first_response.respond(accept())?;
            let ids = std::collections::BTreeSet::from([completions.recv().await.unwrap().elicitation_id.to_string(), completions.recv().await.unwrap().elicitation_id.to_string()]);
            assert_eq!(ids, std::collections::BTreeSet::from([first_url.elicitation_id.to_string(), second_url.elicitation_id.to_string()]));
            first_ack.await?; second_ack.await?;
            Ok(())
        })).await.unwrap().unwrap();
    gateway.abort();
    agent.abort();
}

#[tokio::test]
async fn a_disconnected_pilots_permission_is_reoffered_and_its_late_answer_cannot_approve_it() {
    let (address, gateway, agent, mut reports) = fixture("cancel").await;
    let (seen, mut requests) = mpsc::unbounded_channel();
    let (opened_tx, opened_rx) = oneshot::channel();
    let (disconnect, disconnected) = oneshot::channel();
    let first = tokio::spawn(
        Client
            .v2()
            .on_receive_request(
                async move |_: v2::RequestPermissionRequest,
                            responder: Responder<v2::RequestPermissionResponse>,
                            _: V2ConnectionTo<Agent>| {
                    seen.send(responder).unwrap();
                    Ok(())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(
                HttpClient::new(format!("ws://{address}")).unwrap(),
                async move |cx| {
                    let opened = initialize(&cx, false).await?;
                    opened_tx.send(opened.clone()).unwrap();
                    let _pending = cx
                        .send_request(
                            v2::PromptRequest::new(opened.session_id, vec!["go".into()])
                                .meta(controlled(&opened.meta, "disconnect")),
                        )
                        .block_task();
                    disconnected.await.unwrap();
                    Ok(())
                },
            ),
    );
    timeout(Duration::from_secs(10), async {
        let opened = opened_rx.await.unwrap();
        let stale = requests.recv().await.unwrap();
        disconnect.send(()).unwrap();
        first.await.unwrap().unwrap();
        let (reoffered, mut offers) = mpsc::unbounded_channel();
        Client
            .v2()
            .on_receive_request(
                async move |_: v2::RequestPermissionRequest,
                            responder: Responder<v2::RequestPermissionResponse>,
                            _: V2ConnectionTo<Agent>| {
                    reoffered.send(()).unwrap();
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
                        v2::Implementation::new("replacement", "1"),
                    ))
                    .block_task()
                    .await?;
                    cx.send_request(v2::ResumeSessionRequest::new(
                        opened.session_id,
                        std::env::current_dir().unwrap(),
                    ))
                    .block_task()
                    .await?;
                    offers.recv().await.unwrap();
                    let _ = stale.respond(v2::RequestPermissionResponse::new(
                        v2::RequestPermissionOutcome::Selected(v2::SelectedPermissionOutcome::new(
                            "allow",
                        )),
                    ));
                    assert_eq!(
                        reports.recv().await.unwrap()["outcome"]["outcome"],
                        "cancelled"
                    );
                    Ok(())
                },
            )
            .await
            .unwrap();
    })
    .await
    .unwrap();
    gateway.abort();
    agent.abort();
}

#[tokio::test]
async fn a_less_capable_pilot_can_take_over_and_safely_cancel_an_unrenderable_pending_form() {
    let (address, gateway, agent, mut reports) = fixture("unsupported").await;
    let (tx, mut requests) = mpsc::unbounded_channel();
    let (opened_tx, opened_rx) = oneshot::channel();
    let (finish, finished) = oneshot::channel();
    let first = tokio::spawn(
        Client
            .v2()
            .on_receive_request(
                async move |request: v2::CreateElicitationRequest,
                            responder: Responder<v2::CreateElicitationResponse>,
                            _: V2ConnectionTo<Agent>| {
                    tx.send((request, responder)).unwrap();
                    Ok(())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(
                HttpClient::new(format!("ws://{address}")).unwrap(),
                async move |cx| {
                    let opened = initialize(&cx, true).await?;
                    opened_tx.send(opened.clone()).unwrap();
                    cx.send_request(
                        v2::PromptRequest::new(opened.session_id, vec!["go".into()])
                            .meta(controlled(&opened.meta, "takeover")),
                    )
                    .block_task()
                    .await?;
                    finished.await.unwrap();
                    Ok(())
                },
            ),
    );
    timeout(Duration::from_secs(10), async {
        let opened = opened_rx.await.unwrap();
        let (_, stale) = requests.recv().await.unwrap();
        let cancelled = stale.cancellation();
        Client
            .v2()
            .connect_with(
                HttpClient::new(format!("ws://{address}")).unwrap(),
                async move |cx| {
                    cx.send_request(v2::InitializeRequest::new(
                        ProtocolVersion::V2,
                        v2::Implementation::new("minimal", "1"),
                    ))
                    .block_task()
                    .await?;
                    let resumed = cx
                        .send_request(v2::ResumeSessionRequest::new(
                            opened.session_id,
                            std::env::current_dir().unwrap(),
                        ))
                        .block_task()
                        .await?;
                    assert_eq!(resumed.meta.unwrap()["hive"]["role"], "pilot");
                    cancelled.cancelled().await;
                    let _ = stale.respond(
                        serde_json::from_value(
                            json!({"action":"accept","content":{"approved":true}}),
                        )
                        .unwrap(),
                    );
                    assert_eq!(reports.recv().await.unwrap()["action"], "cancel");
                    Ok(())
                },
            )
            .await
            .unwrap();
        finish.send(()).unwrap();
        first.await.unwrap().unwrap();
    })
    .await
    .unwrap();
    gateway.abort();
    agent.abort();
}
