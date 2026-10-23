//! Deterministic ACP v2 stdio agent; stdout belongs exclusively to JSON-RPC.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use agent_client_protocol::schema::{v1, v2};
use agent_client_protocol::{Agent, Client, Error, Responder, Stdio, V2ConnectionTo};
use serde::{Deserialize, Serialize};

#[derive(Default, Deserialize, Serialize)]
struct State {
    next: u64,
    sessions: HashMap<v2::SessionId, (v2::AbsolutePath, Vec<v2::SessionUpdate>)>,
}

fn save(state: &State) -> agent_client_protocol::Result<()> {
    let Some(path) = std::env::var_os("HIVE_MOCK_STATE") else {
        return Ok(());
    };
    std::fs::write(
        path,
        serde_json::to_vec(state).map_err(Error::into_internal_error)?,
    )
    .map_err(Error::into_internal_error)
}

fn notify(
    cx: &V2ConnectionTo<Client>,
    id: &v2::SessionId,
    update: v2::SessionUpdate,
) -> agent_client_protocol::Result<()> {
    cx.send_notification(v2::UpdateSessionNotification::new(id.clone(), update))
}

fn invalid(message: &str) -> Error {
    Error::invalid_params().data(message.to_owned())
}

#[tokio::main]
async fn main() -> agent_client_protocol::Result<()> {
    if std::env::args().nth(1).as_deref() == Some("v1") {
        return serve_v1().await;
    }
    let state = Arc::new(Mutex::new(
        std::env::var_os("HIVE_MOCK_STATE")
            .and_then(|path| std::fs::read(path).ok())
            .and_then(|data| serde_json::from_slice::<State>(&data).ok())
            .unwrap_or_default(),
    ));
    Agent
        .v2()
        .name("hive-mock-agent")
        .on_receive_request(
            async |request: v2::InitializeRequest,
                   responder: Responder<v2::InitializeResponse>,
                   _cx: V2ConnectionTo<Client>| {
                responder.respond(
                    v2::InitializeResponse::new(
                        request.protocol_version,
                        v2::Implementation::new("hive-mock-agent", "0.1.0"),
                    )
                    .capabilities(
                        v2::AgentCapabilities::new().session(v2::SessionCapabilities::new()),
                    ),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = state.clone();
                async move |request: v2::NewSessionRequest,
                            responder: Responder<v2::NewSessionResponse>,
                            cx: V2ConnectionTo<Client>| {
                    let id = {
                        let mut state = state.lock().map_err(|_| Error::internal_error())?;
                        state.next += 1;
                        let id = v2::SessionId::new(format!("mock-{}", state.next));
                        state.sessions.insert(id.clone(), (request.cwd, Vec::new()));
                        save(&state)?;
                        id
                    };
                    responder.respond(v2::NewSessionResponse::new(id.clone()))?;
                    notify(
                        &cx,
                        &id,
                        v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(
                            v2::IdleStateUpdate::new(),
                        )),
                    )
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = state.clone();
                async move |request: v2::ListSessionsRequest,
                            responder: Responder<v2::ListSessionsResponse>,
                            _cx: V2ConnectionTo<Client>| {
                    let state = state.lock().map_err(|_| Error::internal_error())?;
                    let entries = state
                        .sessions
                        .iter()
                        .filter(|(_, (cwd, _))| {
                            request.cwd.as_ref().is_none_or(|filter| filter == cwd)
                        })
                        .map(|(id, (cwd, _))| v2::SessionInfo::new(id.clone(), cwd.clone()))
                        .collect();
                    responder.respond(v2::ListSessionsResponse::new(entries))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = state.clone();
                async move |request: v2::ResumeSessionRequest,
                            responder: Responder<v2::ResumeSessionResponse>,
                            cx: V2ConnectionTo<Client>| {
                    let updates = {
                        let state = state.lock().map_err(|_| Error::internal_error())?;
                        let (_, history) = state
                            .sessions
                            .get(&request.session_id)
                            .ok_or_else(|| invalid("unknown session"))?;
                        if request.replay_from.is_some() {
                            history.clone()
                        } else {
                            Vec::new()
                        }
                    };
                    for update in updates {
                        notify(&cx, &request.session_id, update)?;
                    }
                    responder.respond(v2::ResumeSessionResponse::new())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = state.clone();
                async move |request: v2::CloseSessionRequest,
                            responder: Responder<v2::CloseSessionResponse>,
                            _cx: V2ConnectionTo<Client>| {
                    let mut state = state.lock().map_err(|_| Error::internal_error())?;
                    state
                        .sessions
                        .remove(&request.session_id)
                        .ok_or_else(|| invalid("unknown session"))?;
                    save(&state)?;
                    responder.respond(v2::CloseSessionResponse::new())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = state.clone();
                async move |request: v2::PromptRequest,
                            responder: Responder<v2::PromptResponse>,
                            cx: V2ConnectionTo<Client>| {
                    let text = request
                        .prompt
                        .iter()
                        .filter_map(|block| match block {
                            v2::ContentBlock::Text(text) => Some(text.text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join(" ");
                    let (user_id, agent_id) = {
                        let mut state = state.lock().map_err(|_| Error::internal_error())?;
                        state.next += 1;
                        let seq = state.next;
                        if !state.sessions.contains_key(&request.session_id) {
                            return responder.respond_with_error(invalid("unknown session"));
                        }
                        (
                            v2::MessageId::new(format!("user-{seq}")),
                            v2::MessageId::new(format!("agent-{seq}")),
                        )
                    };
                    responder.respond(v2::PromptResponse::new(user_id.clone()))?;
                    let user = v2::SessionUpdate::UserMessage(
                        v2::UserMessage::new(user_id).content(request.prompt),
                    );
                    notify(&cx, &request.session_id, user.clone())?;
                    notify(
                        &cx,
                        &request.session_id,
                        v2::SessionUpdate::StateUpdate(v2::StateUpdate::Running(
                            v2::RunningStateUpdate::new(),
                        )),
                    )?;
                    if text.contains("activity-sequence") {
                        notify(
                            &cx,
                            &request.session_id,
                            v2::SessionUpdate::ToolCallUpdate(
                                v2::ToolCallUpdate::new("mock-tool")
                                    .title("Check files")
                                    .status(v2::ToolCallStatus::InProgress),
                            ),
                        )?;
                        notify(
                            &cx,
                            &request.session_id,
                            v2::SessionUpdate::PlanUpdate(v2::PlanUpdate::new(
                                v2::PlanUpdateContent::items(
                                    "mock-plan",
                                    vec![v2::PlanEntry::new(
                                        "Check files",
                                        v2::PlanEntryPriority::High,
                                        v2::PlanEntryStatus::InProgress,
                                    )],
                                ),
                            )),
                        )?;
                        notify(
                            &cx,
                            &request.session_id,
                            v2::SessionUpdate::ToolCallUpdate(
                                v2::ToolCallUpdate::new("mock-tool")
                                    .status(v2::ToolCallStatus::Completed)
                                    .raw_output(serde_json::json!({"result": "ok"})),
                            ),
                        )?;
                    }
                    let reply = format!(
                        "{}Echo: {text}",
                        std::env::var("HIVE_MOCK_LABEL").unwrap_or_default()
                    );
                    notify(
                        &cx,
                        &request.session_id,
                        v2::SessionUpdate::AgentMessageChunk(v2::ContentChunk::new(
                            reply.clone().into(),
                            agent_id.clone(),
                        )),
                    )?;
                    let snapshot = v2::SessionUpdate::AgentMessage(
                        v2::AgentMessage::new(agent_id).content(vec![reply.into()]),
                    );
                    notify(&cx, &request.session_id, snapshot.clone())?;
                    let mut idle = v2::IdleStateUpdate::new().stop_reason(v2::StopReason::EndTurn);
                    if text.contains("usage-fixture") {
                        idle = idle.usage(v2::Usage::new(120, 80, 40).cached_read_tokens(Some(20)));
                        notify(
                            &cx,
                            &request.session_id,
                            v2::SessionUpdate::UsageUpdate(
                                v2::UsageUpdate::new(120, 200_000)
                                    .cost(v2::Cost::new(0.004, "USD")),
                            ),
                        )?;
                    }
                    notify(
                        &cx,
                        &request.session_id,
                        v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(idle)),
                    )?;
                    if text.contains("activity-sequence") {
                        let cx = cx.clone();
                        let session = request.session_id.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
                            let _ = notify(
                                &cx,
                                &session,
                                v2::SessionUpdate::Other(v2::OtherSessionUpdate::new(
                                    "_hive_task",
                                    std::collections::BTreeMap::from([
                                        ("taskId".into(), serde_json::json!("background")),
                                        ("status".into(), serde_json::json!("completed")),
                                    ]),
                                )),
                            );
                        });
                    }
                    if let Some((_, history)) = state
                        .lock()
                        .map_err(|_| Error::internal_error())?
                        .sessions
                        .get_mut(&request.session_id)
                    {
                        history.extend([user, snapshot]);
                    }
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async |request: v2::CancelSessionNotification, cx: V2ConnectionTo<Client>| {
                notify(
                    &cx,
                    &request.session_id,
                    v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(
                        v2::IdleStateUpdate::new().stop_reason(v2::StopReason::Cancelled),
                    )),
                )
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_to(Stdio::new())
        .await
}

async fn serve_v1() -> agent_client_protocol::Result<()> {
    let sessions = Arc::new(Mutex::new(0u64));
    Agent
        .builder()
        .on_receive_request(
            async |request: v1::InitializeRequest,
                   responder: Responder<v1::InitializeResponse>,
                   _cx| {
                responder.respond(
                    v1::InitializeResponse::new(request.protocol_version)
                        .agent_capabilities(v1::AgentCapabilities::new()),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let sessions = sessions.clone();
                async move |_request: v1::NewSessionRequest,
                            responder: Responder<v1::NewSessionResponse>,
                            _cx| {
                    let mut sessions = sessions.lock().map_err(|_| Error::internal_error())?;
                    *sessions += 1;
                    responder.respond(v1::NewSessionResponse::new(format!("mock-v1-{sessions}")))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async |request: v1::PromptRequest,
                   responder: Responder<v1::PromptResponse>,
                   cx: agent_client_protocol::ConnectionTo<Client>| {
                let text = request
                    .prompt
                    .iter()
                    .filter_map(|block| match block {
                        v1::ContentBlock::Text(text) => Some(text.text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                if text.contains("activity-sequence") {
                    cx.send_notification(v1::SessionNotification::new(
                        request.session_id.clone(),
                        v1::SessionUpdate::ToolCall(v1::ToolCall::new("v1-tool", "Check files")),
                    ))?;
                    cx.send_notification(v1::SessionNotification::new(
                        request.session_id.clone(),
                        v1::SessionUpdate::Plan(v1::Plan::new(vec![v1::PlanEntry::new(
                            "Check files",
                            v1::PlanEntryPriority::High,
                            v1::PlanEntryStatus::InProgress,
                        )])),
                    ))?;
                    cx.send_notification(v1::SessionNotification::new(
                        request.session_id.clone(),
                        v1::SessionUpdate::ToolCallUpdate(v1::ToolCallUpdate::new(
                            "v1-tool",
                            v1::ToolCallUpdateFields::new().status(v1::ToolCallStatus::Failed),
                        )),
                    ))?;
                }
                cx.send_notification(v1::SessionNotification::new(
                    request.session_id,
                    v1::SessionUpdate::AgentMessageChunk(v1::ContentChunk::new(
                        format!("Echo: {text}").into(),
                    )),
                ))?;
                let mut response = v1::PromptResponse::new(v1::StopReason::EndTurn);
                if text.contains("usage-fixture") {
                    response =
                        response.usage(v1::Usage::new(120, 80, 40).cached_read_tokens(Some(20)));
                }
                responder.respond(response)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_to(Stdio::new())
        .await
}
