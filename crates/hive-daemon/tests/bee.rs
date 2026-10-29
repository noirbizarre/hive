//! Independent ACPv2 interoperability, using https://github.com/noirbizarre/bee/blob/main/docs/scenarios.md.

#[path = "support/bee.rs"]
mod bee;

use agent_client_protocol::{Error, schema::v2};
use bee::{Event, controlled, native, run, text};
use serde_json::{Value, json};
use std::collections::BTreeMap;

fn assert_sequences(events: &[Value]) {
    let sequences: Vec<_> = events
        .iter()
        .map(|event| {
            event["_meta"]["hive"]["seq"]
                .as_u64()
                .expect("Hive must sequence every live update")
        })
        .collect();
    assert!(
        sequences.windows(2).all(|pair| pair[0] < pair[1]),
        "Live updates must have strictly increasing sequences"
    );
}

fn transcript(events: &[Value]) -> Vec<Value> {
    events
        .iter()
        .filter(|event| {
            matches!(
                event["update"]["sessionUpdate"].as_str(),
                Some("user_message" | "agent_message" | "tool_call_update")
            )
        })
        .map(|event| event["update"].clone())
        .collect()
}

#[tokio::test]
async fn completed_conversations_and_tools_reopen_once_in_order_without_new_work() {
    for scenario in ["success", "permission"] {
        run(scenario, move |gateway| async move {
            for (from, to) in [("ws", "http"), ("http", "ws")] {
                let storage = gateway.clone();
                let (tx, rx) = tokio::sync::oneshot::channel();
                gateway
                    .client(from, false, async move |mut peer| {
                        let opened = peer.open(storage.cwd.clone()).await?;
                        let mut live = Vec::new();
                        for key in ["first", "second"] {
                            let receipt = peer.prompt(&opened, key).await?;
                            if scenario == "permission" {
                                loop {
                                    match peer.event().await {
                                        Event::Update(update) => {
                                            live.push(serde_json::to_value(update).unwrap())
                                        }
                                        Event::Permission(_, responder) => {
                                            responder.respond(
                                                v2::RequestPermissionResponse::new(
                                                    v2::RequestPermissionOutcome::Selected(
                                                        v2::SelectedPermissionOutcome::new(
                                                            "allow_once",
                                                        ),
                                                    ),
                                                ),
                                            )?;
                                            break;
                                        }
                                        _ => panic!("Expected the live fixture permission request"),
                                    }
                                }
                            }
                            live.extend(peer.idle("end_turn").await);
                            storage.journal(&opened.session_id, key, &receipt, "completed");
                        }
                        let (_, snapshot) = peer
                            .attach(
                                opened.session_id.clone(),
                                storage.cwd.clone(),
                                Some(v2::ReplayFrom::Start(v2::ReplayFromStart::new())),
                                true,
                            )
                            .await?;
                        let expected = transcript(&snapshot);
                        let messages: Vec<_> = expected
                            .iter()
                            .filter(|update| update["sessionUpdate"] != "tool_call_update")
                            .collect();
                        assert_eq!(messages.len(), 4);
                        let users = native(&live, "user_message");
                        let chunks = native(&live, "agent_message_chunk");
                        for (index, pair) in messages.as_chunks::<2>().0.iter().enumerate() {
                            assert_eq!(pair[0]["sessionUpdate"], "user_message");
                            assert_eq!(pair[0]["messageId"], users[index]["messageId"]);
                            assert_eq!(pair[0]["content"], users[index]["content"]);
                            assert_eq!(pair[1]["sessionUpdate"], "agent_message");
                            let id = &pair[1]["messageId"];
                            let content: String = chunks
                                .iter()
                                .filter(|chunk| &chunk["messageId"] == id)
                                .map(|chunk| chunk["content"]["text"].as_str().unwrap())
                                .collect();
                            assert_eq!(pair[1]["content"][0]["text"], content);
                        }
                        let identities: std::collections::BTreeSet<_> = messages
                            .iter()
                            .map(|message| {
                                (
                                    message["sessionUpdate"].to_string(),
                                    message["messageId"].to_string(),
                                )
                            })
                            .collect();
                        assert_eq!(identities.len(), 4);
                        if scenario == "permission" {
                            let tools = native(&snapshot, "tool_call_update");
                            assert_eq!(tools.len(), 2);
                            assert!(tools.iter().all(|tool| tool["status"] == "completed"));
                        }
                        let evidence = storage.identity_and_receipts(&opened.session_id);
                        assert_eq!(evidence["id"], opened.session_id.to_string());
                        assert!(!evidence["nativeId"].as_str().unwrap().is_empty());
                        assert_eq!(evidence["cwd"], storage.cwd.to_str().unwrap());
                        assert_eq!(evidence["receipts"].as_array().unwrap().len(), 2);
                        tx.send((opened.session_id, expected, evidence)).unwrap();
                        Ok(())
                    })
                    .await?;
                let (id, expected, evidence) = rx.await.map_err(Error::into_internal_error)?;
                // Each connection starts with an empty view, as a page reload does; snapshots
                // must be complete and identical rather than append another copy of history.
                for scheme in [to, from, to] {
                    let id = id.clone();
                    let expected = expected.clone();
                    let evidence = evidence.clone();
                    let storage = gateway.clone();
                    gateway
                        .client(scheme, false, async move |mut peer| {
                            let listed = peer
                                .cx
                                .send_request(v2::ListSessionsRequest::new())
                                .block_task()
                                .await?;
                            assert_eq!(
                                listed.sessions.len(),
                                1,
                                "reopening must not create another conversation"
                            );
                            let entry = listed
                                .sessions
                                .iter()
                                .find(|entry| entry.session_id == id)
                                .unwrap();
                            assert_eq!(
                                serde_json::to_value(entry).unwrap()["cwd"],
                                storage.cwd.to_str().unwrap()
                            );
                            let (attached, replay) = peer
                                .attach(
                                    id.clone(),
                                    storage.cwd.clone(),
                                    Some(v2::ReplayFrom::Start(v2::ReplayFromStart::new())),
                                    false,
                                )
                                .await?;
                            assert_eq!(transcript(&replay), expected);
                            assert!(
                                replay
                                    .iter()
                                    .all(|event| event["sessionId"] == id.to_string())
                            );
                            assert!(native(&replay, "agent_message_chunk").is_empty());
                            assert!(native(&replay, "user_message_chunk").is_empty());
                            assert!(
                                native(&replay, "state_update")
                                    .iter()
                                    .all(|state| state["state"] == "idle")
                            );
                            assert_eq!(attached.meta.as_ref().unwrap()["hive"]["running"], false);
                            assert_eq!(storage.identity_and_receipts(&id), evidence);
                            Ok(())
                        })
                        .await?;
                }
                let storage = gateway.clone();
                gateway
                    .client(to, false, async move |mut peer| {
                        let (attached, _) =
                            peer.attach(id.clone(), storage.cwd, None, false).await?;
                        let opened = v2::NewSessionResponse::new(id).meta(attached.meta);
                        peer.prompt(&opened, "third").await?;
                        if scenario == "permission" {
                            loop {
                                match peer.event().await {
                                    Event::Update(_) => {}
                                    Event::Permission(_, responder) => {
                                        responder.respond(v2::RequestPermissionResponse::new(
                                            v2::RequestPermissionOutcome::Selected(
                                                v2::SelectedPermissionOutcome::new("allow_once"),
                                            ),
                                        ))?;
                                        break;
                                    }
                                    _ => panic!(
                                        "Expected a permission only after the explicit new prompt"
                                    ),
                                }
                            }
                        }
                        peer.idle("end_turn").await;
                        peer.close(&opened).await?;
                        Ok(())
                    })
                    .await?;
            }
            Ok(())
        })
        .await;
    }
}

#[tokio::test]
async fn streamed_turns_are_correlated_and_idempotent_on_both_transports() {
    run("success", |gateway| async move {
        for scheme in ["ws", "http"] {
            let storage = gateway.clone();
            gateway
                .client(scheme, false, async move |mut peer| {
                    let opened = peer.open(storage.cwd.clone()).await?;
                    let first = peer.prompt(&opened, "first").await?;
                    let mut events = peer.idle("end_turn").await;
                    assert_eq!(
                        native(&events, "user_message")[0]["messageId"],
                        first.message_id.to_string()
                    );
                    assert_eq!(native(&events, "agent_message_chunk").len(), 2);
                    assert_eq!(text(&events), "Hello from Bee. Scenario complete.");
                    storage.journal(&opened.session_id, "first", &first, "completed");
                    assert_eq!(
                        peer.prompt(&opened, "first").await?.message_id,
                        first.message_id
                    );
                    assert!(
                        peer.cx
                            .send_request(
                                v2::PromptRequest::new(
                                    opened.session_id.clone(),
                                    vec!["Conflicting content".into()]
                                )
                                .meta(controlled(&opened.meta, "first"))
                            )
                            .block_task()
                            .await
                            .is_err()
                    );
                    let second = peer.prompt(&opened, "second").await?;
                    assert_ne!(first.message_id, second.message_id);
                    events.extend(peer.idle("end_turn").await);
                    // The second terminal event provides a deterministic boundary for detecting duplicate turns.
                    assert_eq!(native(&events, "user_message").len(), 2);
                    assert_eq!(native(&events, "agent_message_chunk").len(), 4);
                    assert_eq!(
                        native(&events, "state_update")
                            .iter()
                            .filter(|state| state["state"] == "running")
                            .count(),
                        2
                    );
                    assert_sequences(&events);
                    assert!(
                        events
                            .iter()
                            .all(|event| event["sessionId"] == opened.session_id.to_string())
                    );
                    let (attached, replay) = peer
                        .attach(
                            opened.session_id,
                            storage.cwd,
                            Some(v2::ReplayFrom::Start(v2::ReplayFromStart::new())),
                            true,
                        )
                        .await?;
                    let state = &attached.meta.as_ref().unwrap()["hive"];
                    assert_eq!(state["running"], false);
                    assert_ne!(state["recoveryRequired"], true);
                    assert_eq!(native(&replay, "user_message").len(), 2);
                    assert_eq!(native(&replay, "agent_message").len(), 2);
                    Ok(())
                })
                .await?;
        }
        Ok(())
    })
    .await;
}

#[tokio::test]
async fn structured_plan_replacements_reach_events_snapshots_and_replay_on_both_transports() {
    run("plan", |gateway| async move {
        for scheme in ["ws", "http"] {
            let workspace = gateway.clone();
            gateway
                .client(scheme, false, async move |mut peer| {
                    let opened = peer.open(workspace.cwd.clone()).await?;
                    peer.prompt(&opened, "plan-one").await?;
                    let events = peer.idle("end_turn").await;
                    assert_sequences(&events);
                    let plans = native(&events, "plan_update");
                    assert_eq!(plans.len(), 2);
                    let id = plans[0]["plan"]["planId"].as_str().unwrap().to_owned();
                    assert_eq!(plans[1]["plan"]["planId"], id);
                    assert_eq!(
                        plans[0]["plan"]["entries"],
                        json!([
                            {"content":"Inspect fixture","priority":"high","status":"in_progress"},
                            {"content":"Verify fixture","priority":"medium","status":"pending"}
                        ])
                    );
                    assert!(
                        plans[1]["plan"]["entries"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .all(|entry| entry["status"] == "completed")
                    );
                    let cursor = events
                        .iter()
                        .find(|event| event["update"]["sessionUpdate"] == "plan_update")
                        .unwrap()["_meta"]["hive"]["seq"]
                        .as_u64()
                        .unwrap();
                    // An independent observer exercises Hive attachment state without transferring control.
                    let observer_workspace = workspace.clone();
                    let observed_id = opened.session_id.clone();
                    let expected = plans[1]["plan"].clone();
                    let first_id = id.clone();
                    workspace
                        .client(scheme, false, async move |mut observer| {
                            let (attached, full) = observer
                                .attach(
                                    observed_id.clone(),
                                    observer_workspace.cwd.clone(),
                                    Some(v2::ReplayFrom::Start(v2::ReplayFromStart::new())),
                                    true,
                                )
                                .await?;
                            let snapshot = &attached.meta.as_ref().unwrap()["hive"]["planSnapshot"];
                            assert_eq!(snapshot["support"], "supported");
                            assert_eq!(snapshot["observation"], "observed");
                            assert_eq!(snapshot["plans"][&id]["plan"], expected);
                            let retained = native(&full, "plan_update");
                            assert_eq!(retained.len(), 1);
                            assert_eq!(retained[0]["plan"], expected);
                            let (_, replay) = observer
                                .attach(
                                    observed_id,
                                    observer_workspace.cwd,
                                    Some(v2::ReplayFrom::Other(v2::OtherReplayFrom::new(
                                        "_hive_seq",
                                        BTreeMap::from([("seq".into(), json!(cursor))]),
                                    ))),
                                    true,
                                )
                                .await?;
                            assert_eq!(
                                native(&replay, "plan_update")
                                    .iter()
                                    .map(|plan| &plan["plan"])
                                    .collect::<Vec<_>>(),
                                plans.iter().map(|plan| &plan["plan"]).collect::<Vec<_>>()
                            );
                            let replay_sequences: Vec<_> = replay
                                .iter()
                                .filter(|event| event["update"]["sessionUpdate"] == "plan_update")
                                .map(|event| event["_meta"]["hive"]["seq"].clone())
                                .collect();
                            assert_eq!(
                                replay_sequences,
                                events
                                    .iter()
                                    .filter(
                                        |event| event["update"]["sessionUpdate"] == "plan_update"
                                    )
                                    .map(|event| event["_meta"]["hive"]["seq"].clone())
                                    .collect::<Vec<_>>()
                            );
                            Ok(())
                        })
                        .await?;
                    peer.prompt(&opened, "plan-two").await?;
                    let second = peer.idle("end_turn").await;
                    assert_ne!(
                        native(&second, "plan_update")[0]["plan"]["planId"],
                        first_id
                    );
                    Ok(())
                })
                .await?;
        }
        Ok(())
    })
    .await;
}

#[tokio::test]
async fn cancellation_and_active_close_clean_up_live_sessions_on_both_transports() {
    run("cancel", |gateway| async move {
        for scheme in ["ws", "http"] {
            let storage = gateway.clone();
            gateway
                .client(scheme, false, async move |mut peer| {
                    let opened = peer.open(storage.cwd.clone()).await?;
                    let receipt = peer.prompt(&opened, "cancelled").await?;
                    peer.ready().await;
                    assert!(peer.prompt(&opened, "busy").await.is_err());
                    peer.cx.send_notification(
                        v2::CancelSessionNotification::new(opened.session_id.clone())
                            .meta(opened.meta.clone()),
                    )?;
                    peer.idle("cancelled").await;
                    storage.journal(&opened.session_id, "cancelled", &receipt, "cancelled");
                    peer.prompt(&opened, "close-active").await?;
                    peer.ready().await;
                    let closed = peer.close(&opened).await?;
                    assert_eq!(
                        native(&closed, "state_update")
                            .iter()
                            .filter(|state| state["state"] == "idle"
                                && state["stopReason"] == "cancelled")
                            .count(),
                        1
                    );
                    let listed = peer
                        .cx
                        .send_request(v2::ListSessionsRequest::new())
                        .block_task()
                        .await?;
                    assert!(
                        !listed
                            .sessions
                            .iter()
                            .any(|entry| entry.session_id == opened.session_id)
                    );
                    Ok(())
                })
                .await?;
        }
        Ok(())
    })
    .await;
}

#[tokio::test]
async fn disconnect_and_cross_transport_resume_preserve_the_running_worker() {
    run("cancel", |gateway| async move {
        let cwd = gateway.cwd.clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        gateway
            .client("ws", false, async move |mut peer| {
                let opened = peer.open(cwd).await?;
                peer.prompt(&opened, "before-disconnect").await?;
                peer.ready().await;
                let _ = tx.send(opened.session_id);
                Ok(())
            })
            .await?;
        let id = rx.await.map_err(Error::into_internal_error)?;
        let cwd = gateway.cwd.clone();
        gateway
            .client("http", false, async move |mut peer| {
                let (resumed, events) = peer
                    .attach(
                        id.clone(),
                        cwd,
                        Some(v2::ReplayFrom::Start(v2::ReplayFromStart::new())),
                        false,
                    )
                    .await?;
                assert_eq!(resumed.meta.as_ref().unwrap()["hive"]["running"], true);
                assert!(native(&events, "agent_message").iter().any(|message| {
                    message["content"].as_array().is_some_and(|content| {
                        content
                            .iter()
                            .any(|block| block["text"] == "Ready for cancellation.")
                    })
                }));
                peer.cx.send_notification(
                    v2::CancelSessionNotification::new(id.clone()).meta(resumed.meta.clone()),
                )?;
                peer.idle("cancelled").await;
                // A resumed session remains usable, rather than being replaced by an empty downstream session.
                peer.cx
                    .send_request(
                        v2::PromptRequest::new(id.clone(), vec!["Again".into()])
                            .meta(controlled(&resumed.meta, "after-resume")),
                    )
                    .block_task()
                    .await?;
                peer.ready().await;
                peer.cx
                    .send_notification(v2::CancelSessionNotification::new(id).meta(resumed.meta))?;
                peer.idle("cancelled").await;
                Ok(())
            })
            .await
    })
    .await;
}

#[tokio::test]
async fn accepted_errors_preserve_partial_output_and_allow_another_turn_on_both_transports() {
    run("error", |gateway| async move {
        for scheme in ["ws", "http"] {
            let storage = gateway.clone();
            gateway
                .client(scheme, false, async move |mut peer| {
                    let opened = peer.open(storage.cwd.clone()).await?;
                    for key in ["error-one", "error-two"] {
                        let receipt = peer.prompt(&opened, key).await?;
                        let events = peer.idle("error").await;
                        assert_eq!(text(&events), "Partial output before the controlled error.");
                        // Hive publishes its controlled diagnostic after the authoritative idle event.
                        loop {
                            let event = peer.update().await;
                            if event["update"]["sessionUpdate"] == "_hive_error" {
                                assert_eq!(
                                    event["update"]["code"],
                                    "hive::agent::foreground_failed"
                                );
                                break;
                            }
                        }
                        storage.journal(&opened.session_id, key, &receipt, "failed");
                        assert_eq!(
                            peer.prompt(&opened, key).await?.message_id,
                            receipt.message_id
                        );
                    }
                    let (attached, replay) = peer
                        .attach(
                            opened.session_id,
                            storage.cwd,
                            Some(v2::ReplayFrom::Other(v2::OtherReplayFrom::new(
                                "_hive_seq",
                                BTreeMap::from([("seq".into(), json!(1))]),
                            ))),
                            true,
                        )
                        .await?;
                    assert_eq!(native(&replay, "_hive_error").len(), 2);
                    assert_ne!(
                        attached.meta.as_ref().unwrap()["hive"]["recoveryRequired"],
                        true
                    );
                    Ok(())
                })
                .await?;
        }
        Ok(())
    })
    .await;
}

#[tokio::test]
async fn permissions_correlate_tool_subjects_and_dialog_outcomes() {
    run("permission", |gateway| async move {
        let cwd = gateway.cwd.clone();
        gateway
            .client("ws", false, async move |mut peer| {
                let opened = peer.open(cwd).await?;
                for (key, choice, expected) in [
                    ("approve", Some("allow_once"), "Permission approved."),
                    ("reject", Some("reject_once"), "Permission denied."),
                    ("dismiss", None, "Permission denied."),
                ] {
                    peer.prompt(&opened, key).await?;
                    let mut before = Vec::new();
                    loop {
                        match peer.event().await {
                            Event::Update(update) => {
                                before.push(serde_json::to_value(update).unwrap())
                            }
                            Event::Permission(request, responder) => {
                                let value = serde_json::to_value(request).unwrap();
                                assert_eq!(
                                    value["subject"]["toolCall"]["toolCallId"],
                                    native(&before, "tool_call_update")[0]["toolCallId"]
                                );
                                assert!(
                                    native(&before, "state_update")
                                        .iter()
                                        .any(|state| state["state"] == "requires_action")
                                );
                                let outcome =
                                    choice.map_or(v2::RequestPermissionOutcome::Cancelled, |id| {
                                        v2::RequestPermissionOutcome::Selected(
                                            v2::SelectedPermissionOutcome::new(id),
                                        )
                                    });
                                responder.respond(v2::RequestPermissionResponse::new(outcome))?;
                                break;
                            }
                            _ => panic!("Expected permission interaction"),
                        }
                    }
                    let events = peer.idle("end_turn").await;
                    assert_eq!(text(&events), expected);
                    assert_eq!(
                        native(&events, "tool_call_update").last().unwrap()["status"],
                        if choice == Some("allow_once") {
                            "completed"
                        } else {
                            "failed"
                        }
                    );
                }
                Ok(())
            })
            .await
    })
    .await;
}

#[tokio::test]
async fn forms_honor_answers_declines_and_negotiated_client_support() {
    run("input", |gateway| async move {
        for supported in [true, false] {
            let cwd = gateway.cwd.clone();
            gateway.client("http", supported, async move |mut peer| {
                let opened = peer.open(cwd).await?;
                for action in ["accept", "decline", "cancel"] {
                    peer.prompt(&opened, action).await?;
                    if supported {
                        let mut tools = Vec::new();
                        loop {
                            match peer.event().await {
                                Event::Update(update) => tools.push(serde_json::to_value(update).unwrap()),
                                Event::Form(request, responder) => {
                                    let value = serde_json::to_value(request).unwrap();
                                    assert_eq!(value["mode"], "form");
                                    assert_eq!(value["sessionId"], opened.session_id.to_string());
                                    assert_eq!(value["toolCallId"], native(&tools, "tool_call_update")[0]["toolCallId"]);
                                    let response: v2::CreateElicitationResponse = serde_json::from_value(if action == "accept" { json!({"action":"accept","content":{"answer":"blue"}}) } else { json!({"action":action}) }).unwrap();
                                    responder.respond(response)?;
                                    break;
                                }
                                _ => panic!("Expected form interaction"),
                            }
                        }
                    }
                    let events = peer.idle("end_turn").await;
                    let accepted = supported && action == "accept";
                    assert_eq!(text(&events), if accepted { "Input accepted." } else { "Input unavailable, declined, or invalid." });
                    assert_eq!(native(&events, "tool_call_update").last().unwrap()["status"], if accepted { "completed" } else { "failed" });
                }
                Ok(())
            }).await?;
        }
        Ok(())
    }).await;
}

#[tokio::test]
async fn session_cancel_and_close_withdraw_pending_interactions_and_finish_cancelled() {
    for (scenario, scheme) in [("permission", "ws"), ("input", "http")] {
        run(scenario, move |gateway| async move {
            let cwd = gateway.cwd.clone();
            gateway
                .client(scheme, true, async move |mut peer| {
                    let opened = peer.open(cwd).await?;
                    peer.prompt(&opened, "pending").await?;
                    let held = loop {
                        match peer.event().await {
                            Event::Update(_) => {}
                            event @ (Event::Permission(..) | Event::Form(..)) => break event,
                            Event::Boundary => panic!("Unexpected response boundary"),
                        }
                    };
                    let pending = match &held {
                        Event::Permission(_, responder) => responder.cancellation(),
                        Event::Form(_, responder) => responder.cancellation(),
                        _ => unreachable!(),
                    };
                    if scenario == "permission" {
                        peer.cx.send_notification(
                            v2::CancelSessionNotification::new(opened.session_id.clone())
                                .meta(opened.meta.clone()),
                        )?;
                    }
                    let closed = if scenario == "input" {
                        Some(peer.close(&opened).await?)
                    } else {
                        None
                    };
                    tokio::time::timeout(std::time::Duration::from_secs(5), pending.cancelled())
                        .await
                        .expect("Pending interaction must be withdrawn");
                    let events = match closed {
                        Some(events) => events,
                        None => peer.idle("cancelled").await,
                    };
                    assert!(
                        native(&events, "tool_call_update")
                            .iter()
                            .any(|tool| tool["status"] == "cancelled")
                    );
                    assert_eq!(
                        native(&events, "state_update")
                            .iter()
                            .filter(|state| state["stopReason"] == "cancelled")
                            .count(),
                        1
                    );
                    Ok(())
                })
                .await
        })
        .await;
    }
}
