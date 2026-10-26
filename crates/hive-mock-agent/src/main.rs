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
    #[serde(default)]
    models: HashMap<String, String>,
    #[serde(default)]
    modes: HashMap<String, String>,
    #[serde(default)]
    settings: HashMap<String, String>,
    #[serde(default)]
    v1_sessions: HashMap<String, (std::path::PathBuf, Vec<v1::SessionUpdate>)>,
    #[serde(default)]
    restores: Vec<String>,
    // Pending responders are process-local; restart fixtures must not persist transport handles.
    #[serde(skip)]
    pending_v1: HashMap<String, (Responder<v1::PromptResponse>, String)>,
}

fn load() -> State {
    std::env::var_os("HIVE_MOCK_STATE")
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|data| serde_json::from_slice(&data).ok())
        .unwrap_or_default()
}

fn model_options(current: &str) -> Vec<v2::SessionConfigOption> {
    // Deliberately opaque values catch clients that try to derive provider identity from IDs.
    serde_json::from_value(serde_json::json!([{
        "configId": "model", "name": "Model", "category": "model", "type": "select", "currentValue": current,
        "options": [
            {"groupId": "first-provider", "name": "First provider", "options": [{"value": "fast", "name": "Fast"}]},
            {"groupId": "second-provider", "name": "Second provider", "options": [{"value": "careful", "name": "Careful"}, {"value": "rejected", "name": "Unavailable"}]}
        ]
    }])).expect("valid mock configuration")
}

fn session_options(state: &State, id: &str) -> Vec<v2::SessionConfigOption> {
    let mut options = model_options(state.models.get(id).map(String::as_str).unwrap_or("fast"));
    if std::env::var_os("HIVE_MOCK_SETTINGS").is_some() {
        for (config, category, name) in [
            ("mode", "mode", "Approval preset"),
            (
                "collaboration_mode",
                "collaboration_mode",
                "Collaboration mode",
            ),
        ] {
            let current = state
                .settings
                .get(&format!("{id}:{config}"))
                .map(String::as_str)
                .unwrap_or("default");
            options.push(serde_json::from_value(serde_json::json!({
                "configId":config,"category":category,"name":name,"type":"select","currentValue":current,
                "options":[{"value":"default","name":"Default"},{"value":"plan","name":"Plan"},{"value":"rejected","name":"Unavailable"}]
            })).expect("valid native settings"));
        }
    }
    options
}

fn v1_options(options: Vec<v2::SessionConfigOption>) -> Vec<v1::SessionConfigOption> {
    options
        .into_iter()
        .map(|option| {
            let mut value = serde_json::to_value(option).expect("configuration serializes");
            value["id"] = value.as_object_mut().unwrap().remove("configId").unwrap();
            for choice in value["options"].as_array_mut().unwrap() {
                if let Some(group) = choice.as_object_mut().unwrap().remove("groupId") {
                    choice["group"] = group;
                }
            }
            serde_json::from_value(value).expect("valid v1 configuration")
        })
        .collect()
}

fn v1_model_options(current: &str) -> Vec<v1::SessionConfigOption> {
    v1_options(model_options(current))
}

fn legacy_modes(current: &str) -> Option<v1::SessionModeState> {
    std::env::var_os("HIVE_MOCK_MODES").map(|_| {
        serde_json::from_value(serde_json::json!({
            "currentModeId": current, "availableModes": [
                {"id":"build", "name":"Build", "description":"Execute work"},
                {"id":"plan", "name":"Plan", "description":"Plan work"},
                {"id":"rejected", "name":"Unavailable"}
            ]
        }))
        .expect("valid mock modes")
    })
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

fn plan_sequence() -> Vec<v2::SessionUpdate> {
    // Distinct metadata layers catch gateways that retain entries but lose the native envelope.
    let entry = |content: &str, status: &str| {
        serde_json::json!({
            "content": content, "priority": "high", "status": status, "_meta": {"entry": content}
        })
    };
    [
        serde_json::json!({"type":"items", "planId":"steps", "entries":[entry("Inspect", "in_progress"), entry("Verify", "pending")], "_meta":{"plan":true}}),
        serde_json::json!({"type":"items", "planId":"steps", "entries":[entry("Verify", "in_progress"), entry("Inspect", "completed")]}),
        serde_json::json!({"type":"items", "planId":"steps", "entries":[entry("Verify", "cancelled")]}),
        serde_json::json!({"type":"items", "planId":"empty", "entries":[]}),
        serde_json::json!({"type":"_future", "planId":"custom", "description":"Opaque plan", "payload":{"ordered":[2,1]}}),
    ].into_iter().map(|plan| serde_json::from_value(serde_json::json!({
        "sessionUpdate":"plan_update", "plan":plan, "_meta":{"update":true}
    })).expect("valid mock plan")).collect()
}

#[tokio::main]
async fn main() -> agent_client_protocol::Result<()> {
    if std::env::args().nth(1).as_deref() == Some("v1") {
        return serve_v1().await;
    }
    let state = Arc::new(Mutex::new(load()));
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
                            _cx: V2ConnectionTo<Client>| {
                    let id = {
                        let mut state = state.lock().map_err(|_| Error::internal_error())?;
                        state.next += 1;
                        let id = v2::SessionId::new(format!("mock-{}", state.next));
                        state.sessions.insert(id.clone(), (request.cwd, Vec::new()));
                        save(&state)?;
                        id
                    };
                    // No unsolicited `idle` here. Hive only routes updates for a session once it has
                    // handled this response, so such an idle was either dropped or, on a loaded
                    // machine, arrived after the client's first prompt and closed that turn with
                    // no usage. Tests could not tell the two outcomes apart.
                    responder.respond(v2::NewSessionResponse::new(id.clone()).config_options(
                        session_options(
                            &*state.lock().map_err(|_| Error::internal_error())?,
                            &id.to_string(),
                        ),
                    ))
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
                    match std::env::var("HIVE_MOCK_RESTORE").as_deref() {
                        Ok("unimplemented") => {
                            return responder.respond_with_error(Error::method_not_found());
                        }
                        Ok("reject") => {
                            return responder
                                .respond_with_error(invalid("history replay unavailable"));
                        }
                        Ok("missing") => {
                            return responder
                                .respond_with_error(invalid("saved session not found"));
                        }
                        _ => {}
                    }
                    let updates = {
                        let mut state = state.lock().map_err(|_| Error::internal_error())?;
                        let (cwd, history) = state
                            .sessions
                            .get(&request.session_id)
                            .ok_or_else(|| invalid("unknown session"))?;
                        if cwd != &request.cwd {
                            return responder.respond_with_error(invalid("cwd mismatch"));
                        }
                        let updates = if request.replay_from.is_some() {
                            if !matches!(request.replay_from, Some(v2::ReplayFrom::Start(_))) {
                                return responder.respond_with_error(invalid("unsupported cursor"));
                            }
                            if std::env::var("HIVE_MOCK_RESTORE").as_deref() == Ok("empty") {
                                Vec::new()
                            } else {
                                let mut updates = history.clone();
                                if std::env::var("HIVE_MOCK_RESTORE").as_deref() == Ok("partial") {
                                    // Retention may legally discard a prefix without advertising its extent.
                                    updates = updates.into_iter().skip(2).collect();
                                }
                                updates
                            }
                        } else {
                            Vec::new()
                        };
                        state.restores.push(
                            if request.replay_from.is_some() {
                                "v2:start"
                            } else {
                                "v2:none"
                            }
                            .into(),
                        );
                        save(&state)?;
                        updates
                    };
                    for update in updates {
                        notify(&cx, &request.session_id, update)?;
                    }
                    // ACPv2 requires current state after replay and before the resume response.
                    if std::env::var("HIVE_MOCK_RESTORE").as_deref() != Ok("no-state") {
                        notify(
                            &cx,
                            &request.session_id,
                            v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(
                                v2::IdleStateUpdate::new(),
                            )),
                        )?;
                    }
                    let model = state
                        .lock()
                        .map_err(|_| Error::internal_error())?
                        .models
                        .get(&request.session_id.to_string())
                        .cloned()
                        .unwrap_or_else(|| "fast".into());
                    responder.respond(v2::ResumeSessionResponse::new().config_options(
                        if std::env::var_os("HIVE_MOCK_SETTINGS").is_some() {
                            session_options(
                                &*state.lock().map_err(|_| Error::internal_error())?,
                                &request.session_id.to_string(),
                            )
                        } else {
                            model_options(&model)
                        },
                    ))?;
                    if std::env::var("HIVE_MOCK_RESTORE").as_deref() == Ok("live") {
                        // No sleeps: these messages must follow the response even if its waiter has not run.
                        notify(
                            &cx,
                            &request.session_id,
                            v2::SessionUpdate::UsageUpdate(v2::UsageUpdate::new(321, 200_000)),
                        )?;
                        notify(
                            &cx,
                            &request.session_id,
                            v2::SessionUpdate::ConfigOptionUpdate(v2::ConfigOptionUpdate::new(
                                model_options("careful"),
                            )),
                        )?;
                        cx.prepare_request(v2::RequestPermissionRequest::new(
                            request.session_id,
                            "Post-resume permission",
                            vec![],
                        ))
                        .detach()?;
                    }
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = state.clone();
                async move |request: v2::SetSessionConfigOptionRequest,
                            responder: Responder<v2::SetSessionConfigOptionResponse>,
                            cx: V2ConnectionTo<Client>| {
                    let v2::SessionConfigOptionValue::Id { value } = request.value else {
                        return responder.respond_with_error(invalid("expected an id"));
                    };
                    let config = request.config_id.to_string();
                    let valid = if config == "model" {
                        ["fast", "careful"].contains(&value.to_string().as_str())
                    } else {
                        std::env::var_os("HIVE_MOCK_SETTINGS").is_some()
                            && ["mode", "collaboration_mode"].contains(&config.as_str())
                            && ["default", "plan"].contains(&value.to_string().as_str())
                    };
                    if !valid
                        || request
                            .meta
                            .as_ref()
                            .is_some_and(|meta| meta.contains_key("hive"))
                    {
                        return responder.respond_with_error(invalid(
                            "invalid choice or leaked Hive credentials",
                        ));
                    }
                    let options = {
                        let mut state = state.lock().map_err(|_| Error::internal_error())?;
                        if !state.sessions.contains_key(&request.session_id) {
                            return responder
                                .respond_with_error(invalid("unknown downstream session"));
                        }
                        if config == "model" {
                            state
                                .models
                                .insert(request.session_id.to_string(), value.to_string());
                        } else {
                            state.settings.insert(
                                format!("{}:{config}", request.session_id),
                                value.to_string(),
                            );
                        }
                        save(&state)?;
                        session_options(&state, &request.session_id.to_string())
                    };
                    if std::env::var_os("HIVE_MOCK_PAUSE_CONFIG").is_some() {
                        notify(
                            &cx,
                            &request.session_id,
                            v2::SessionUpdate::Other(v2::OtherSessionUpdate::new(
                                "_mock_configuring",
                                Default::default(),
                            )),
                        )?;
                        // Announce dispatch before delaying so race tests synchronize on an event, not a guessed admission time.
                        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                    }
                    if value.to_string() == "careful" {
                        notify(
                            &cx,
                            &request.session_id,
                            v2::SessionUpdate::ConfigOptionUpdate(v2::ConfigOptionUpdate::new(
                                options.clone(),
                            )),
                        )?;
                    }
                    responder.respond(v2::SetSessionConfigOptionResponse::new(options))?;
                    if std::env::var("HIVE_MOCK_SETTINGS").as_deref() == Ok("post-response") {
                        let mut state = state.lock().map_err(|_| Error::internal_error())?;
                        state.settings.insert(
                            format!("{}:collaboration_mode", request.session_id),
                            "default".into(),
                        );
                        notify(
                            &cx,
                            &request.session_id,
                            v2::SessionUpdate::ConfigOptionUpdate(v2::ConfigOptionUpdate::new(
                                session_options(&state, &request.session_id.to_string()),
                            )),
                        )?;
                    }
                    Ok(())
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
                    let mut acknowledgement = v2::PromptResponse::new(user_id.clone());
                    if std::env::var_os("HIVE_MOCK_ACK_META").is_some() {
                        acknowledgement = acknowledgement.meta(v2::Meta::from_iter([(
                            "private".into(),
                            serde_json::json!("acknowledgement metadata sentinel"),
                        )]));
                    }
                    responder.respond(acknowledgement)?;
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
                    if text.contains("native-failure") {
                        let idle: v2::SessionUpdate = serde_json::from_value(serde_json::json!({
                            "sessionUpdate":"state_update", "state":"idle", "stopReason":"error",
                            "error":{"code":-32603,"message":"synthetic sk-must-not-leak", "data":{"token":"sk-must-not-leak"}}
                        })).map_err(Error::into_internal_error)?;
                        // Repeated current-state reports are not two failed turns.
                        notify(&cx, &request.session_id, idle.clone())?;
                        notify(&cx, &request.session_id, idle)?;
                        return Ok(());
                    }
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
                    let plans = if text.contains("plan-sequence") {
                        plan_sequence()
                    } else {
                        Vec::new()
                    };
                    for plan in &plans {
                        notify(&cx, &request.session_id, plan.clone())?;
                    }
                    if text.contains("plan-flood") {
                        // Evict native plan events without changing their current state.
                        for _ in 0..270 {
                            notify(
                                &cx,
                                &request.session_id,
                                v2::SessionUpdate::Other(v2::OtherSessionUpdate::new(
                                    "_mock_padding",
                                    Default::default(),
                                )),
                            )?;
                        }
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
                    {
                        // Persist before idle so restart tests cannot race the agent's last write.
                        let mut state = state.lock().map_err(|_| Error::internal_error())?;
                        if let Some((_, history)) = state.sessions.get_mut(&request.session_id) {
                            history.extend([user, snapshot]);
                            // Some agents do not retain plans; tests must not mistake that for Hive history.
                            if std::env::var_os("HIVE_MOCK_FORGET_PLANS").is_none() {
                                history.extend(plans);
                            }
                        }
                        save(&state)?;
                    }
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
    let state = Arc::new(Mutex::new(load()));
    Agent
        .builder()
        .on_receive_request(
            async |request: v1::InitializeRequest,
                   responder: Responder<v1::InitializeResponse>,
                   _cx| {
                responder.respond(
                    v1::InitializeResponse::new(request.protocol_version).agent_capabilities(
                        v1::AgentCapabilities::new()
                            .load_session(
                                std::env::var("HIVE_MOCK_RESTORE").as_deref() != Ok("resume-only")
                                    && std::env::var("HIVE_MOCK_RESTORE").as_deref() != Ok("none"),
                            )
                            .session_capabilities(
                                v1::SessionCapabilities::new().resume(
                                    (std::env::var("HIVE_MOCK_RESTORE").as_deref()
                                        != Ok("load-only")
                                        && std::env::var("HIVE_MOCK_RESTORE").as_deref()
                                            != Ok("none"))
                                    .then(v1::SessionResumeCapabilities::new),
                                ),
                            ),
                    ),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = state.clone();
                async move |request: v1::NewSessionRequest,
                            responder: Responder<v1::NewSessionResponse>,
                            _cx| {
                    let mut state = state.lock().map_err(|_| Error::internal_error())?;
                    state.next += 1;
                    let id = format!("mock-v1-{}", state.next);
                    state
                        .v1_sessions
                        .insert(id.clone(), (request.cwd, Vec::new()));
                    save(&state)?;
                    let legacy = legacy_modes("build");
                    responder.respond(
                        v1::NewSessionResponse::new(id.clone())
                            .config_options(if legacy.is_some() {
                                None
                            } else {
                                Some(v1_options(session_options(&state, &id)))
                            })
                            .modes(legacy),
                    )
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = state.clone();
                async move |request: v1::LoadSessionRequest,
                            responder: Responder<v1::LoadSessionResponse>,
                            cx: agent_client_protocol::ConnectionTo<Client>| {
                    let mut state = state.lock().map_err(|_| Error::internal_error())?;
                    let (cwd, history) = state
                        .v1_sessions
                        .get(&request.session_id.to_string())
                        .ok_or_else(|| invalid("unknown session"))?;
                    if cwd != &request.cwd {
                        return responder.respond_with_error(invalid("cwd mismatch"));
                    }
                    for update in history {
                        if std::env::var("HIVE_MOCK_RESTORE").as_deref() != Ok("empty") {
                            cx.send_notification(v1::SessionNotification::new(
                                request.session_id.clone(),
                                update.clone(),
                            ))?;
                        }
                    }
                    state.restores.push("v1:load".into());
                    save(&state)?;
                    let mode = state
                        .modes
                        .get(&request.session_id.to_string())
                        .map(String::as_str)
                        .unwrap_or("build");
                    responder.respond(
                        v1::LoadSessionResponse::new()
                            .modes(legacy_modes(mode))
                            .config_options(std::env::var_os("HIVE_MOCK_SETTINGS").map(|_| {
                                v1_options(session_options(&state, &request.session_id.to_string()))
                            })),
                    )?;
                    if std::env::var("HIVE_MOCK_RESTORE").as_deref() == Ok("live") {
                        cx.send_notification(v1::SessionNotification::new(
                            request.session_id.clone(),
                            v1::SessionUpdate::UsageUpdate(v1::UsageUpdate::new(321, 200_000)),
                        ))?;
                        cx.send_notification(v1::SessionNotification::new(
                            request.session_id.clone(),
                            v1::SessionUpdate::ConfigOptionUpdate(v1::ConfigOptionUpdate::new(
                                v1_model_options("careful"),
                            )),
                        ))?;
                        cx.prepare_request(v1::RequestPermissionRequest::new(
                            request.session_id,
                            v1::ToolCallUpdate::new(
                                "post-resume",
                                v1::ToolCallUpdateFields::new().title("Post-resume permission"),
                            ),
                            vec![],
                        ))
                        .detach()?;
                    }
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = state.clone();
                async move |request: v1::ResumeSessionRequest,
                            responder: Responder<v1::ResumeSessionResponse>,
                            _cx: agent_client_protocol::ConnectionTo<Client>| {
                    let mut state = state.lock().map_err(|_| Error::internal_error())?;
                    let (cwd, _) = state
                        .v1_sessions
                        .get(&request.session_id.to_string())
                        .ok_or_else(|| invalid("unknown session"))?;
                    if cwd != &request.cwd {
                        return responder.respond_with_error(invalid("cwd mismatch"));
                    }
                    state.restores.push("v1:resume".into());
                    save(&state)?;
                    let mode = state
                        .modes
                        .get(&request.session_id.to_string())
                        .map(String::as_str)
                        .unwrap_or("build");
                    responder.respond(v1::ResumeSessionResponse::new().modes(legacy_modes(mode)))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = state.clone();
                async move |request: v1::SetSessionConfigOptionRequest,
                            responder: Responder<v1::SetSessionConfigOptionResponse>,
                            cx: agent_client_protocol::ConnectionTo<Client>| {
                    let v1::SessionConfigOptionValue::ValueId { value } = request.value else {
                        return responder.respond_with_error(invalid("expected an id"));
                    };
                    let config = request.config_id.to_string();
                    let valid = if config == "model" {
                        ["fast", "careful"].contains(&value.to_string().as_str())
                    } else {
                        std::env::var_os("HIVE_MOCK_SETTINGS").is_some()
                            && ["mode", "collaboration_mode"].contains(&config.as_str())
                            && ["default", "plan"].contains(&value.to_string().as_str())
                    };
                    if !request.session_id.to_string().starts_with("mock-v1-")
                        || !valid
                        || request
                            .meta
                            .as_ref()
                            .is_some_and(|meta| meta.contains_key("hive"))
                    {
                        return responder.respond_with_error(invalid("invalid downstream request"));
                    }
                    let options = {
                        let mut state = state.lock().map_err(|_| Error::internal_error())?;
                        if config == "model" {
                            state
                                .models
                                .insert(request.session_id.to_string(), value.to_string());
                        } else {
                            state.settings.insert(
                                format!("{}:{config}", request.session_id),
                                value.to_string(),
                            );
                        }
                        save(&state)?;
                        v1_options(session_options(&state, &request.session_id.to_string()))
                    };
                    if std::env::var_os("HIVE_MOCK_PAUSE_CONFIG").is_some() {
                        // A standard update crosses the compatibility adapter without requiring a test-only gateway extension.
                        cx.send_notification(v1::SessionNotification::new(
                            request.session_id.clone(),
                            v1::SessionUpdate::ConfigOptionUpdate(v1::ConfigOptionUpdate::new(
                                options.clone(),
                            )),
                        ))?;
                        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                    }
                    if value.to_string() == "careful" {
                        cx.send_notification(v1::SessionNotification::new(
                            request.session_id.clone(),
                            v1::SessionUpdate::ConfigOptionUpdate(v1::ConfigOptionUpdate::new(
                                options.clone(),
                            )),
                        ))?;
                    }
                    responder.respond(v1::SetSessionConfigOptionResponse::new(options))?;
                    if std::env::var("HIVE_MOCK_SETTINGS").as_deref() == Ok("post-response") {
                        let mut state = state.lock().map_err(|_| Error::internal_error())?;
                        state.settings.insert(
                            format!("{}:collaboration_mode", request.session_id),
                            "default".into(),
                        );
                        cx.send_notification(v1::SessionNotification::new(
                            request.session_id.clone(),
                            v1::SessionUpdate::ConfigOptionUpdate(v1::ConfigOptionUpdate::new(
                                v1_options(session_options(
                                    &state,
                                    &request.session_id.to_string(),
                                )),
                            )),
                        ))?;
                    }
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = state.clone();
                async move |request: v1::SetSessionModeRequest,
                            responder: Responder<v1::SetSessionModeResponse>,
                            cx: agent_client_protocol::ConnectionTo<Client>| {
                    let mut state = state.lock().map_err(|_| Error::internal_error())?;
                    if legacy_modes("build").is_none()
                        || !state
                            .v1_sessions
                            .contains_key(&request.session_id.to_string())
                        || !["build", "plan"].contains(&request.mode_id.to_string().as_str())
                        || request
                            .meta
                            .as_ref()
                            .is_some_and(|meta| meta.contains_key("hive"))
                    {
                        return responder
                            .respond_with_error(invalid("invalid native mode request"));
                    }
                    // Optional notification reports a different mode to exercise acknowledgement ordering.
                    let current = if std::env::var("HIVE_MOCK_MODES").as_deref() == Ok("notify") {
                        "build".to_owned()
                    } else {
                        request.mode_id.to_string()
                    };
                    state
                        .modes
                        .insert(request.session_id.to_string(), current.clone());
                    save(&state)?;
                    if std::env::var("HIVE_MOCK_MODES").as_deref() == Ok("notify") {
                        cx.send_notification(v1::SessionNotification::new(
                            request.session_id,
                            v1::SessionUpdate::CurrentModeUpdate(v1::CurrentModeUpdate::new(
                                current,
                            )),
                        ))?;
                    }
                    responder.respond(v1::SetSessionModeResponse::new())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = state.clone();
                async move |request: v1::PromptRequest,
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
                            v1::SessionUpdate::ToolCall(v1::ToolCall::new(
                                "v1-tool",
                                "Check files",
                            )),
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
                    if text.contains("fail-prompt") {
                        return responder.respond_with_error(
                            Error::internal_error()
                                .data("synthetic failure with secret sk-must-not-leak"),
                        );
                    }
                    if text.contains("wait-for-cancel") {
                        state
                            .lock()
                            .map_err(|_| Error::internal_error())?
                            .pending_v1
                            .insert(request.session_id.to_string(), (responder, text));
                        // Readiness follows registration, so the test can cancel without a timing race.
                        cx.send_notification(v1::SessionNotification::new(
                            request.session_id,
                            v1::SessionUpdate::AgentMessageChunk(v1::ContentChunk::new(
                                "ready-for-cancel".into(),
                            )),
                        ))?;
                        return Ok(());
                    }
                    let reply = v1::SessionUpdate::AgentMessageChunk(v1::ContentChunk::new(
                        format!("Echo: {text}").into(),
                    ));
                    {
                        let mut state = state.lock().map_err(|_| Error::internal_error())?;
                        let (_, history) = state
                            .v1_sessions
                            .get_mut(&request.session_id.to_string())
                            .ok_or_else(|| invalid("unknown session"))?;
                        history.push(v1::SessionUpdate::UserMessageChunk(v1::ContentChunk::new(
                            text.clone().into(),
                        )));
                        history.push(reply.clone());
                        save(&state)?;
                    }
                    cx.send_notification(v1::SessionNotification::new(request.session_id, reply))?;
                    let reason = if text.contains("stop-max-tokens") {
                        v1::StopReason::MaxTokens
                    } else if text.contains("stop-max-turn-requests") {
                        v1::StopReason::MaxTurnRequests
                    } else if text.contains("stop-refusal") {
                        v1::StopReason::Refusal
                    } else {
                        v1::StopReason::EndTurn
                    };
                    let mut response = v1::PromptResponse::new(reason);
                    if text.contains("usage-fixture") {
                        response = response
                            .usage(v1::Usage::new(120, 80, 40).cached_read_tokens(Some(20)));
                    }
                    responder.respond(response)
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            {
                let state = state.clone();
                async move |request: v1::CancelNotification,
                            _cx: agent_client_protocol::ConnectionTo<Client>| {
                    let pending = state
                        .lock()
                        .map_err(|_| Error::internal_error())?
                        .pending_v1
                        .remove(&request.session_id.to_string());
                    if let Some((responder, text)) = pending {
                        if text.contains("cancel-error") {
                            responder.respond_with_error(
                                Error::internal_error().data("sk-must-not-leak"),
                            )?;
                        } else {
                            // Sending cancel does not prove cancellation: the downstream outcome does.
                            let reason = if text.contains("cancel-end-turn") {
                                v1::StopReason::EndTurn
                            } else {
                                v1::StopReason::Cancelled
                            };
                            responder.respond(v1::PromptResponse::new(reason))?;
                        }
                    }
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_to(Stdio::new())
        .await
}
