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
                .on_receive_request(async move |_: v2::NewSessionRequest, responder: Responder<v2::NewSessionResponse>, _: V2ConnectionTo<Client>| {
                    let mut response = v2::NewSessionResponse::new(v2::SessionId::new("native"));
                    if mode == "config" {
                        response = response.config_options(serde_json::from_value(json!([{"configId":"model","name":"Model","type":"select","currentValue":"fast","options":[{"value":"fast","name":"Fast"},{"value":"slow","name":"Slow"}]}])).unwrap());
                    }
                    responder.respond(response)
                }, agent_client_protocol::on_receive_request!())
                .on_receive_request(async |request: v2::CloseSessionRequest, responder: Responder<v2::CloseSessionResponse>, cx: V2ConnectionTo<Client>| {
                    cx.send_notification(v2::UpdateSessionNotification::new(request.session_id, v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(v2::IdleStateUpdate::new().stop_reason(v2::StopReason::Cancelled)))))?;
                    responder.respond(v2::CloseSessionResponse::new())
                }, agent_client_protocol::on_receive_request!())
                .on_receive_request({
                    let report = report.clone();
                    async move |_: v2::SetSessionConfigOptionRequest, responder: Responder<v2::SetSessionConfigOptionResponse>, cx: V2ConnectionTo<Client>| {
                        report.send(json!("config-started")).unwrap();
                        let report = report.clone();
                        cx.spawn(async move {
                            responder.cancellation().cancelled().await;
                            report.send(json!("config-cancelled")).unwrap();
                            responder.respond_with_error(Error::request_cancelled())
                        })
                    }
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
                            "cancel" | "close-permission" => json!(peer.send_request(permission("cancel me")).block_task().await.unwrap()),
                            "request" | "unsupported" | "close-form" => {
                                let scope = if mode == "request" || mode == "close-form" { v2::ElicitationRequestScope::new(responder.id().clone()).into() }
                                    else { v2::ElicitationScope::from(v2::ElicitationSessionScope::new(request.session_id.clone())) };
                                let schema = serde_json::from_value(json!({"type":"object","properties":{"approved":{"type":"boolean"}},"required":["approved"]})).unwrap();
                                json!(peer.send_request(v2::CreateElicitationRequest::new(v2::ElicitationFormMode::new(scope, schema), "Approve?")).block_task().await.unwrap())
                            }
                            "url-early" | "url-late" | "close-url" => {
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
                        if mode.starts_with("close-") { return responder.respond_with_error(Error::request_cancelled()); }
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
async fn close_withdraws_permissions_forms_and_urls_before_pending_insertion_on_both_transports() {
    for mode in ["close-permission", "close-form", "close-url"] {
        for transport in ["ws", "http"] {
            let (address, gateway, agent, mut reports) = fixture(mode).await;
            let (tx, mut rx) = mpsc::unbounded_channel();
            let forms = tx.clone();
            timeout(
                Duration::from_secs(10),
                Client
                    .v2()
                    .on_receive_request(
                        async move |_: v2::RequestPermissionRequest,
                                    responder: Responder<v2::RequestPermissionResponse>,
                                    cx: V2ConnectionTo<Agent>| {
                            tx.send(responder.cancellation()).unwrap();
                            cx.spawn(async move {
                                responder.cancellation().cancelled().await;
                                responder.respond_with_error(Error::request_cancelled())
                            })
                        },
                        agent_client_protocol::on_receive_request!(),
                    )
                    .on_receive_request(
                        async move |_: v2::CreateElicitationRequest,
                                    responder: Responder<v2::CreateElicitationResponse>,
                                    cx: V2ConnectionTo<Agent>| {
                            forms.send(responder.cancellation()).unwrap();
                            cx.spawn(async move {
                                responder.cancellation().cancelled().await;
                                responder.respond_with_error(Error::request_cancelled())
                            })
                        },
                        agent_client_protocol::on_receive_request!(),
                    )
                    .connect_with(
                        HttpClient::new(format!("{transport}://{address}")).unwrap(),
                        async move |cx| {
                            let opened = initialize(&cx, true).await?;
                            let prompt = cx
                                .send_request(
                                    v2::PromptRequest::new(
                                        opened.session_id.clone(),
                                        vec!["go".into()],
                                    )
                                    .meta(controlled(&opened.meta, "close")),
                                )
                                .block_task();
                            let cancellation = rx.recv().await.unwrap();
                            cx.send_request(
                                v2::CloseSessionRequest::new(opened.session_id).meta(opened.meta),
                            )
                            .block_task()
                            .await?;
                            cancellation.cancelled().await;
                            assert_eq!(
                                prompt.await.unwrap_err().code,
                                Error::request_cancelled().code
                            );
                            let result = reports.recv().await.unwrap();
                            assert!(result.to_string().contains("cancel"), "{mode}: {result}");
                            Ok::<_, Error>(())
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
}

#[tokio::test]
async fn close_cancels_a_pending_configuration_without_waiting_for_the_setter_on_both_transports() {
    for transport in ["ws", "http"] {
        let (address, gateway, agent, mut reports) = fixture("config").await;
        timeout(
            Duration::from_secs(10),
            Client.v2().connect_with(
                HttpClient::new(format!("{transport}://{address}")).unwrap(),
                async move |cx| {
                    let opened = initialize(&cx, false).await?;
                    let setter = cx
                        .send_request(
                            v2::SetSessionConfigOptionRequest::new(
                                opened.session_id.clone(),
                                v2::SessionConfigId::new("model"),
                                v2::SessionConfigOptionValue::Id {
                                    value: v2::SessionConfigValueId::new("slow"),
                                },
                            )
                            .meta(opened.meta.clone()),
                        )
                        .block_task();
                    assert_eq!(reports.recv().await.unwrap(), "config-started");
                    cx.send_request(
                        v2::CloseSessionRequest::new(opened.session_id).meta(opened.meta),
                    )
                    .block_task()
                    .await?;
                    assert!(setter.await.is_err());
                    assert_eq!(reports.recv().await.unwrap(), "config-cancelled");
                    Ok::<_, Error>(())
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
