//! Real protocol fencing, with explicit expiry after the production watchdog is armed.

use crate::{Broker, Config, config::Profile};
use agent_client_protocol::{
    Agent, Client, Error, Responder, V2ConnectionTo,
    schema::{ProtocolVersion, v2},
};
use agent_client_protocol_http::{AcpHttpServer, HttpClient};
use serde_json::json;
use std::time::Duration;
use tokio::{sync::mpsc, time::timeout};

#[tokio::test]
async fn an_agent_that_never_settles_a_cancelled_dialog_is_fenced_for_recovery() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let downstream = listener.local_addr().unwrap();
    let (report, mut reports) = mpsc::unbounded_channel();
    let agent = tokio::spawn(async move {
        axum::serve(
            listener,
            AcpHttpServer::new(move || {
                let report = report.clone();
                Agent
                    .v2()
                    .on_receive_request(
                        async |request: v2::InitializeRequest,
                               responder: Responder<v2::InitializeResponse>,
                               _: V2ConnectionTo<Client>| {
                            responder.respond(
                                v2::InitializeResponse::new(
                                    request.protocol_version,
                                    v2::Implementation::new("stalled", "1"),
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
                            responder.respond(v2::NewSessionResponse::new("native"))
                        },
                        agent_client_protocol::on_receive_request!(),
                    )
                    .on_receive_request(
                        async move |request: v2::PromptRequest,
                                    responder: Responder<v2::PromptResponse>,
                                    cx: V2ConnectionTo<Client>| {
                            let report = report.clone();
                            cx.clone().spawn(async move {
                                // Running, rather than pre-insertion input, is what arms cancellation fencing.
                                responder.respond(v2::PromptResponse::new("accepted"))?;
                                cx.send_notification(v2::UpdateSessionNotification::new(
                                    request.session_id.clone(),
                                    v2::SessionUpdate::StateUpdate(v2::StateUpdate::Running(
                                        v2::RunningStateUpdate::new(),
                                    )),
                                ))?;
                                let result = cx
                                    .send_request(v2::RequestPermissionRequest::new(
                                        request.session_id,
                                        "Never settled",
                                        vec![v2::PermissionOption::new(
                                            "allow",
                                            "Allow once",
                                            v2::PermissionOptionKind::AllowOnce,
                                        )],
                                    ))
                                    .block_task()
                                    .await;
                                report.send(result.is_ok()).unwrap();
                                Ok(())
                            })
                        },
                        agent_client_protocol::on_receive_request!(),
                    )
            })
            .into_router(),
        )
        .await
        .unwrap();
    });
    let mut config = Config {
        default_agent: Some("stalled".into()),
        ..Config::default()
    };
    config.agents.insert(
        "stalled".into(),
        serde_json::from_value::<Profile>(
            json!({"endpoint":format!("http://{downstream}"),"versions":["v2"]}),
        )
        .unwrap(),
    );
    let directory = tempfile::tempdir().unwrap();
    let broker =
        Broker::persistent(config, directory.path().join("hive/sessions.sqlite3")).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let origin = format!("http://{address}");
    let router = crate::router_with_origin(broker.clone(), Some(&origin));
    let gateway = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let (tx, mut dialogs) = mpsc::unbounded_channel();
    let (updates, mut events) = mpsc::unbounded_channel();
    let result = timeout(
        Duration::from_secs(10),
        Client
            .v2()
            .on_receive_notification(
                async move |update: v2::UpdateSessionNotification, _: V2ConnectionTo<Agent>| {
                    updates.send(update).unwrap();
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
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
                async |cx| {
                    cx.send_request(v2::InitializeRequest::new(
                        ProtocolVersion::V2,
                        v2::Implementation::new("watchdog-test", "1"),
                    ))
                    .block_task()
                    .await?;
                    let opened = cx
                        .send_request(v2::NewSessionRequest::new(std::env::current_dir().unwrap()))
                        .block_task()
                        .await?;
                    let mut meta = opened.meta.clone().unwrap();
                    meta.get_mut("hive").unwrap()["key"] = json!("stalled");
                    cx.send_request(
                        v2::PromptRequest::new(opened.session_id.clone(), vec!["go".into()])
                            .meta(meta),
                    )
                    .block_task()
                    .await?;
                    let dialog = dialogs.recv().await.unwrap();
                    let control = broker.sessions.lock().unwrap()[&opened.session_id.to_string()]
                        .control
                        .clone();
                    let interruption = control.lock().unwrap().native_interruption.clone().unwrap();
                    let mut armed = interruption.subscribe();
                    cx.send_notification(
                        v2::CancelSessionNotification::new(opened.session_id.clone())
                            .meta(opened.meta.clone()),
                    )?;
                    dialog.cancellation().cancelled().await;
                    armed.wait_for(Option::is_some).await.unwrap();
                    // Exact 15-second semantics are covered with paused time in control_tests. Here expiry
                    // is triggered only after real cancellation admission, avoiding virtual-clock/socket races.
                    interruption.send_replace(Some(tokio::time::Instant::now()));
                    loop {
                        let update =
                            serde_json::to_value(events.recv().await.unwrap().update).unwrap();
                        if update["sessionUpdate"] == "_hive_control"
                            && update["recoveryRequired"] == true
                        {
                            break;
                        }
                    }
                    assert!(matches!(
                        reports.try_recv(),
                        Err(mpsc::error::TryRecvError::Empty)
                    ));
                    let unavailable = cx
                        .send_request(v2::ResumeSessionRequest::new(
                            opened.session_id.clone(),
                            std::env::current_dir().unwrap(),
                        ))
                        .block_task()
                        .await
                        .unwrap_err();
                    assert_eq!(unavailable.code, Error::invalid_request().code);
                    assert!(
                        unavailable
                            .data
                            .unwrap()
                            .as_str()
                            .unwrap()
                            .contains("downstream worker exited")
                    );
                    let mut meta = opened.meta.unwrap();
                    meta.get_mut("hive").unwrap()["key"] = json!("new");
                    assert!(
                        cx.send_request(
                            v2::PromptRequest::new(
                                opened.session_id,
                                vec!["do not redispatch".into()]
                            )
                            .meta(meta)
                        )
                        .block_task()
                        .await
                        .is_err()
                    );
                    Ok::<_, Error>(())
                },
            ),
    )
    .await;
    gateway.abort();
    agent.abort();
    result.unwrap().unwrap();
}
