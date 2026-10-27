//! Real process death at durable acceptance boundaries, with execution counts independent of Hive.

use super::*;
use std::{path::Path, process::Stdio};
use tokio::io::{AsyncBufReadExt, BufReader};

fn recovery_config(base: &Path) -> Config {
    let mut config = config_with_agent(
        "mock",
        env!("CARGO_BIN_EXE_hive-test-agent"),
        AgentVersion::V2,
    );
    let env = &mut config.agents.get_mut("mock").unwrap().env;
    env.insert(
        "HIVE_MOCK_STATE".into(),
        base.join("agent.json").display().to_string(),
    );
    if std::env::var_os("LLVM_PROFILE_FILE").is_some() {
        // Killed children cannot flush valid coverage profiles.
        env.insert(
            "LLVM_PROFILE_FILE".into(),
            if cfg!(windows) { "NUL" } else { "/dev/null" }.into(),
        );
    }
    config
}

#[tokio::test]
async fn recovery_subprocess_host() {
    let Some(base) = std::env::var_os("HIVE_RECOVERY_HOST_BASE") else {
        return;
    };
    let base = std::path::PathBuf::from(base);
    let broker =
        hive_daemon::Broker::persistent(recovery_config(&base), base.join("hive/sessions.sqlite3"))
            .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    // The parent waits on a pipe barrier, never on a startup sleep.
    println!(
        "HIVE_RECOVERY_HOST_READY {}",
        listener.local_addr().unwrap()
    );
    axum::serve(listener, hive_daemon::router(broker))
        .await
        .unwrap();
}

#[tokio::test]
async fn process_death_preserves_receipts_uncertainty_and_original_identity_on_both_transports() {
    for transport in ["ws", "http"] {
        for scenario in [
            "recovery-accepted-lost-ack",
            "recovery-completed-lost-ack",
            "recovery-completed-lost-idle",
            "recovery-hold",
        ] {
            let base = std::env::temp_dir().join(format!("hive-boundary-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&base).unwrap();
            let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "recovery::recovery_subprocess_host",
                    "--nocapture",
                ])
                .env("HIVE_RECOVERY_HOST_BASE", &base)
                .env(
                    "LLVM_PROFILE_FILE",
                    if cfg!(windows) { "NUL" } else { "/dev/null" },
                )
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            let mut output = BufReader::new(child.stdout.take().unwrap()).lines();
            let address = timeout(Duration::from_secs(20), async {
                loop {
                    let line = output
                        .next_line()
                        .await
                        .unwrap()
                        .expect("host exited before readiness");
                    if let Some(address) = line.strip_prefix("HIVE_RECOVERY_HOST_READY ") {
                        break address.parse::<SocketAddr>().unwrap();
                    }
                }
            })
            .await
            .unwrap();
            let cwd = std::env::current_dir().unwrap();
            let (updates, mut events) = mpsc::unbounded_channel();
            let (id, receipt) = Client
                .v2()
                .on_receive_notification(
                    async move |update: v2::UpdateSessionNotification,
                                _cx: V2ConnectionTo<Agent>| {
                        let _ = updates.send(update);
                        Ok(())
                    },
                    agent_client_protocol::on_receive_notification!(),
                )
                .connect_with(
                    HttpClient::new(format!("{transport}://{address}")).unwrap(),
                    async |cx| {
                        cx.send_request(v2::InitializeRequest::new(
                            ProtocolVersion::V2,
                            v2::Implementation::new("recovery-boundary", "1"),
                        ))
                        .block_task()
                        .await?;
                        let opened = cx
                            .send_request(v2::NewSessionRequest::new(cwd.clone()))
                            .block_task()
                            .await?;
                        let result = cx
                            .send_request(
                                v2::PromptRequest::new(
                                    opened.session_id.clone(),
                                    vec![scenario.into()],
                                )
                                .meta(controlled(&opened.meta, "original")),
                            )
                            .block_task()
                            .await;
                        let receipt = match result {
                            Ok(response) => Some(response.message_id),
                            Err(error) => {
                                assert_eq!(
                                    error.data.unwrap()["hive"]["code"],
                                    "hive::prompt::outcome_uncertain"
                                );
                                None
                            }
                        };
                        if scenario == "recovery-hold" {
                            // Ensure Hive actually observed running before killing it mid-turn.
                            timeout(Duration::from_secs(10), async {
                                while let Some(update) = events.recv().await {
                                    if matches!(
                                        update.update,
                                        v2::SessionUpdate::StateUpdate(v2::StateUpdate::Running(_))
                                    ) {
                                        break;
                                    }
                                }
                            })
                            .await
                            .unwrap();
                        }
                        Ok((opened.session_id, receipt))
                    },
                )
                .await
                .unwrap();
            child.kill().await.unwrap();
            child.wait().await.unwrap();
            let before: serde_json::Value =
                serde_json::from_slice(&std::fs::read(base.join("agent.json")).unwrap()).unwrap();
            assert_eq!(before["executions"][scenario], 1);
            let native_id = before["sessions"]
                .as_object()
                .unwrap()
                .keys()
                .next()
                .unwrap()
                .clone();

            let database = base.join("hive/sessions.sqlite3");
            let recovered =
                hive_daemon::Broker::persistent(recovery_config(&base), database.clone()).unwrap();
            recovered.reconcile().await;
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let router = hive_daemon::router(recovered.clone());
            let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
            assert_eq!(
                summaries(address).await["sessions"][0]["sessionId"],
                id.to_string()
            );
            Client
                .v2()
                .connect_with(
                    HttpClient::new(format!("{transport}://{address}")).unwrap(),
                    async |cx| {
                        cx.send_request(v2::InitializeRequest::new(
                            ProtocolVersion::V2,
                            v2::Implementation::new("recovery-boundary", "1"),
                        ))
                        .block_task()
                        .await?;
                        let observed = cx
                            .send_request(
                                v2::ResumeSessionRequest::new(id.clone(), cwd.clone())
                                    .replay_from(v2::ReplayFrom::Start(v2::ReplayFromStart::new()))
                                    .meta(v2::Meta::from_iter([(
                                        "hive".into(),
                                        serde_json::json!({"mode":"observe"}),
                                    )])),
                            )
                            .block_task()
                            .await?;
                        assert_eq!(
                            observed.meta.as_ref().unwrap()["hive"]["recoveryRequired"],
                            true
                        );
                        assert_ne!(
                            observed.meta.as_ref().unwrap()["hive"]["transcript"]["completeness"],
                            "complete"
                        );
                        let blocked = cx
                            .send_request(v2::ResumeSessionRequest::new(id.clone(), cwd.clone()))
                            .block_task()
                            .await
                            .unwrap_err();
                        assert_eq!(
                            blocked.data.unwrap()["hive"]["code"],
                            "hive::session::recovery_required"
                        );
                        let owner = cx
                            .send_request(
                                v2::ResumeSessionRequest::new(id.clone(), cwd.clone()).meta(
                                    v2::Meta::from_iter([(
                                        "hive".into(),
                                        serde_json::json!({"recoveryAction":"acknowledge"}),
                                    )]),
                                ),
                            )
                            .block_task()
                            .await?;
                        assert_eq!(
                            owner.meta.as_ref().unwrap()["hive"]["recoveryRequired"],
                            false
                        );
                        let result = cx
                            .send_request(
                                v2::PromptRequest::new(id.clone(), vec![scenario.into()])
                                    .meta(controlled(&owner.meta, "original")),
                            )
                            .block_task()
                            .await;
                        if let Some(receipt) = receipt {
                            assert_eq!(result?.message_id, receipt);
                        } else {
                            let error = result.unwrap_err().data.unwrap();
                            assert_eq!(error["hive"]["code"], "hive::prompt::outcome_uncertain");
                            assert_eq!(error["hive"]["recoveryRequired"], false);
                        }
                        let conflict = cx
                            .send_request(
                                v2::PromptRequest::new(id.clone(), vec!["changed".into()])
                                    .meta(controlled(&owner.meta, "original")),
                            )
                            .block_task()
                            .await
                            .unwrap_err();
                        assert_eq!(
                            conflict.data.unwrap()["hive"]["code"],
                            "hive::prompt::key_conflict"
                        );
                        Ok(())
                    },
                )
                .await
                .unwrap();
            let after: serde_json::Value =
                serde_json::from_slice(&std::fs::read(base.join("agent.json")).unwrap()).unwrap();
            assert_eq!(after["executions"][scenario], 1);
            assert_eq!(
                after["sessions"]
                    .as_object()
                    .unwrap()
                    .keys()
                    .collect::<Vec<_>>(),
                vec![&native_id]
            );
            recovered.shutdown().await;
            server.abort();
            let _ = server.await;
            drop(recovered);
            // Acknowledging the old incident must survive yet another restart without freeing its key.
            let reopened =
                hive_daemon::Broker::persistent(recovery_config(&base), database).unwrap();
            reopened.reconcile().await;
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let router = hive_daemon::router(reopened.clone());
            let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
            assert_eq!(
                summaries(address).await["sessions"][0]["recovery"],
                "reattached"
            );
            reopened.shutdown().await;
            server.abort();
            let _ = server.await;
            std::fs::remove_dir_all(base).unwrap();
        }
    }
}
