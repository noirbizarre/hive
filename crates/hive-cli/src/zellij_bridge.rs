//! A short-request boundary for Zellij's completion-based WebAccess host API.
//! The ACP connection and every downstream worker remain owned by the existing gateway.

use std::{
    collections::{HashMap, VecDeque},
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use agent_client_protocol::{
    Agent, Client, Responder, V2ConnectionTo,
    schema::{ProtocolVersion, v2},
};
use agent_client_protocol_http::HttpClient;
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc, oneshot};

#[derive(Clone)]
struct Server {
    url: String,
    clients: Arc<Mutex<HashMap<String, Peer>>>,
}

struct Peer {
    commands: mpsc::UnboundedSender<Command>,
    last_seen: Instant,
}

struct Command {
    request: Request,
    result: oneshot::Sender<Result<Value, String>>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Request {
    action: String,
    client: Option<String>,
    session_id: Option<String>,
    cwd: Option<String>,
    seq: Option<u64>,
    text: Option<String>,
    key: Option<String>,
    answer: Option<Value>,
}

enum Interaction {
    Permission(
        v2::RequestPermissionRequest,
        oneshot::Sender<v2::RequestPermissionResponse>,
    ),
    Elicitation(
        v2::CreateElicitationRequest,
        oneshot::Sender<v2::CreateElicitationResponse>,
    ),
}

#[derive(Default)]
struct ClientState {
    id: Option<String>,
    cwd: Option<String>,
    owner: Option<v2::Meta>,
    generation: u64,
    running: bool,
    seq: u64,
    updates: VecDeque<Value>,
    interaction: Option<Interaction>,
    // A disconnected acknowledgement must never be retried under a new key.
    pending_prompt: Option<(String, String)>,
    // Keep the last acknowledged key for a WebAccess timeout that hid its response.
    last_prompt: Option<(String, String)>,
}

fn elicitation_answer(
    request: &v2::CreateElicitationRequest,
    raw: Value,
) -> Result<v2::CreateElicitationResponse, String> {
    if raw["action"] != "accept" {
        return serde_json::from_value(raw).map_err(|e| format!("Invalid input: {e}"));
    }
    if let v2::ElicitationMode::Form(form) = &request.mode {
        let values = raw["content"]
            .as_object()
            .ok_or("Enter the requested form fields")?;
        let mut content = std::collections::BTreeMap::new();
        for (key, value) in values {
            let schema = form
                .requested_schema
                .properties
                .get(key)
                .ok_or_else(|| format!("Unknown field: {key}"))?;
            let type_name = serde_json::to_value(schema).map_err(|e| e.to_string())?["type"]
                .as_str()
                .unwrap_or("string")
                .to_owned();
            let text = value
                .as_str()
                .ok_or_else(|| format!("{key} must be text"))?;
            let parsed = match type_name.as_str() {
                "integer" => v2::ElicitationContentValue::Integer(
                    text.parse()
                        .map_err(|_| format!("{key} needs an integer"))?,
                ),
                "number" => v2::ElicitationContentValue::Number(
                    text.parse().map_err(|_| format!("{key} needs a number"))?,
                ),
                "boolean" => v2::ElicitationContentValue::Boolean(
                    text.parse()
                        .map_err(|_| format!("{key} needs true or false"))?,
                ),
                "array" => v2::ElicitationContentValue::StringArray(
                    text.split('|').map(str::to_owned).collect(),
                ),
                _ => v2::ElicitationContentValue::String(text.into()),
            };
            content.insert(key.clone(), parsed);
        }
        for required in form
            .requested_schema
            .required
            .as_deref()
            .unwrap_or_default()
        {
            if !content.contains_key(required) {
                return Err(format!("Required field: {required}"));
            }
        }
        return Ok(v2::CreateElicitationResponse::new(
            v2::ElicitationAction::Accept(v2::ElicitationAcceptAction::new().content(content)),
        ));
    }
    serde_json::from_value(raw).map_err(|e| format!("Invalid input: {e}"))
}

impl ClientState {
    fn pilot(&self) -> bool {
        self.owner
            .as_ref()
            .and_then(|m| m.get("hive"))
            .is_some_and(|h| {
                h["role"] == "pilot" && h["generation"] == self.generation && h["token"].is_string()
            })
    }

    fn snapshot(&self, since: u64) -> Result<Value, String> {
        if self.seq >= since
            && self
                .updates
                .front()
                .and_then(|v| v["seq"].as_u64())
                .is_some_and(|first| since < first)
        {
            return Err("History expired; attach without history explicitly".into());
        }
        let interaction = match &self.interaction {
            Some(Interaction::Permission(req, _)) if self.pilot() => {
                json!({"type":"permission", "request":req})
            }
            Some(Interaction::Elicitation(req, _)) if self.pilot() => {
                json!({"type":"elicitation", "request":req})
            }
            _ => Value::Null,
        };
        Ok(
            json!({"sessionId":self.id, "pilot":self.pilot(), "running":self.running,
            "generation":self.generation, "seq":self.seq, "interaction":interaction,
            "updates":self.updates.iter().filter(|u| u["seq"].as_u64().is_some_and(|n| n >= since)).collect::<Vec<_>>(),
            "pendingPrompt":self.pending_prompt.as_ref().map(|(key,text)| json!({"key":key,"text":text}))}),
        )
    }

    fn accept_update(&mut self, update: v2::UpdateSessionNotification) {
        if self.id.as_deref() != Some(update.session_id.to_string().as_str()) {
            return;
        }
        let Some(seq) = update
            .meta
            .as_ref()
            .and_then(|m| m.get("hive"))
            .and_then(|h| h["seq"].as_u64())
        else {
            return;
        };
        if seq <= self.seq {
            return;
        }
        if let v2::SessionUpdate::StateUpdate(status) = &update.update {
            match status {
                v2::StateUpdate::Running(_) => self.running = true,
                v2::StateUpdate::Idle(_) => self.running = false,
                _ => {}
            }
        }
        if let v2::SessionUpdate::Other(other) = &update.update
            && other.session_update == "_hive_control"
        {
            self.generation = other.fields["generation"]
                .as_u64()
                .unwrap_or(self.generation);
            if !self.pilot() {
                self.owner = None;
                self.interaction = None;
            }
        }
        self.seq = seq;
        self.updates
            .push_back(json!({"seq":seq,"update":update.update}));
        if self.updates.len() > 256 {
            self.updates.pop_front();
        }
    }
}

fn meta(mode: &str, owner: Option<&v2::Meta>) -> v2::Meta {
    let mut value = if mode == "relinquish" {
        owner.cloned().unwrap_or_default()
    } else {
        v2::Meta::new()
    };
    if mode != "pilot" {
        let mut hive = value.get("hive").cloned().unwrap_or_else(|| json!({}));
        hive["mode"] = json!(mode);
        value.insert("hive".into(), hive);
    }
    value
}

async fn resume(
    cx: &V2ConnectionTo<Agent>,
    state: &Arc<Mutex<ClientState>>,
    id: String,
    cwd: String,
    mode: &str,
    live: bool,
) -> Result<Value, String> {
    let (cursor, owner) = {
        let mut guard = state.lock().await;
        let previous = guard.id.as_deref() == Some(&id);
        if !previous {
            // Replay notifications arrive before the resume response; give them their identity now.
            guard.id = Some(id.clone());
            guard.cwd = Some(cwd.clone());
            guard.seq = 0;
            guard.updates.clear();
            guard.owner = None;
            guard.interaction = None;
        }
        (if previous { guard.seq } else { 0 }, guard.owner.clone())
    };
    let mut request = v2::ResumeSessionRequest::new(v2::SessionId::new(id.clone()), cwd.clone())
        .meta(meta(mode, owner.as_ref()));
    if !live {
        let replay = if cursor == 0 {
            v2::ReplayFrom::Start(v2::ReplayFromStart::new())
        } else {
            v2::ReplayFrom::Other(v2::OtherReplayFrom::new(
                "_hive_seq",
                std::collections::BTreeMap::from([("seq".into(), json!(cursor + 1))]),
            ))
        };
        request = request.replay_from(replay);
    }
    let response = cx
        .send_request(request)
        .block_task()
        .await
        .map_err(|e| format!("{e:?}"))?;
    let hive = response
        .meta
        .as_ref()
        .and_then(|m| m.get("hive"))
        .ok_or("Hive resume metadata missing")?;
    let mut guard = state.lock().await;
    if live {
        guard.updates.clear();
        guard.seq = hive["nextSeq"].as_u64().unwrap_or(1).saturating_sub(1);
    }
    guard.id = Some(id);
    guard.cwd = Some(cwd);
    guard.generation = hive["generation"].as_u64().unwrap_or(0);
    guard.running = hive["running"].as_bool().unwrap_or(false);
    guard.owner = response.meta;
    guard.snapshot(
        guard
            .updates
            .front()
            .and_then(|v| v["seq"].as_u64())
            .unwrap_or(guard.seq + 1),
    )
}

async fn command(
    cx: &V2ConnectionTo<Agent>,
    state: &Arc<Mutex<ClientState>>,
    request: Request,
) -> Result<Value, String> {
    match request.action.as_str() {
        "attach" | "takeover" | "relinquish" | "live" => {
            let id = request.session_id.ok_or("Select a session")?;
            let cwd = request.cwd.ok_or("Session cwd is missing from catalog")?;
            let mode = match request.action.as_str() {
                "attach" | "live" => "observe",
                "relinquish" => "relinquish",
                _ => "pilot",
            };
            resume(cx, state, id, cwd, mode, request.action == "live").await
        }
        "poll" => state.lock().await.snapshot(request.seq.unwrap_or(1)),
        "prompt" | "retry" => {
            let (id, owner, key, text) = {
                let mut guard = state.lock().await;
                if !guard.pilot() {
                    return Err("Control changed; take control before prompting".into());
                }
                if guard.running && request.action != "retry" {
                    return Err("Wait for idle before prompting".into());
                }
                let (key, text) = if request.action == "retry" {
                    guard
                        .pending_prompt
                        .clone()
                        .or_else(|| guard.last_prompt.clone())
                        .ok_or("No prompt to retry")?
                } else {
                    let text = request.text.ok_or("Enter a prompt")?;
                    if text.trim().is_empty() {
                        return Err("Enter a prompt".into());
                    }
                    if guard.pending_prompt.is_some() {
                        return Err("Retry the uncertain prompt first".into());
                    }
                    (request.key.ok_or("Prompt key is required")?, text)
                };
                guard.pending_prompt = Some((key.clone(), text.clone()));
                (
                    guard.id.clone().ok_or("Select a session")?,
                    guard.owner.clone().ok_or("Take control first")?,
                    key,
                    text,
                )
            };
            let mut owner = owner;
            owner.get_mut("hive").ok_or("Control metadata missing")?["key"] = json!(key);
            let sent = (key, text.clone());
            let outcome = cx
                .send_request(
                    v2::PromptRequest::new(v2::SessionId::new(id), vec![text.into()]).meta(owner),
                )
                .block_task()
                .await;
            match outcome {
                Ok(_) => {
                    let mut guard = state.lock().await;
                    if guard.pending_prompt.as_ref() == Some(&sent) {
                        guard.pending_prompt = None;
                    }
                    guard.last_prompt = Some(sent);
                    Ok(json!({"accepted":true}))
                }
                Err(error) => Err(format!(
                    "Prompt acknowledgement uncertain: {error:?}; retry with the same key"
                )),
            }
        }
        "cancel" => {
            let guard = state.lock().await;
            if !guard.pilot() {
                return Err("Control changed; take control to cancel".into());
            }
            cx.send_notification(
                v2::CancelSessionNotification::new(v2::SessionId::new(
                    guard.id.clone().ok_or("Select a session")?,
                ))
                .meta(guard.owner.clone().ok_or("Take control first")?),
            )
            .map_err(|e| format!("{e:?}"))?;
            Ok(json!({"sent":true}))
        }
        "answer" => {
            let mut guard = state.lock().await;
            if !guard.pilot() {
                return Err("Control changed; cannot answer".into());
            }
            let answer = request.answer.ok_or("An answer is required")?;
            let interaction = guard
                .interaction
                .as_ref()
                .ok_or("No pending input request")?;
            let validated = match interaction {
                Interaction::Permission(req, _) => {
                    let response = if let Some(option) = answer["optionId"].as_str() {
                        if !req
                            .options
                            .iter()
                            .any(|o| o.option_id.to_string() == option)
                        {
                            return Err("Select a listed option".into());
                        }
                        v2::RequestPermissionResponse::new(v2::RequestPermissionOutcome::Selected(
                            v2::SelectedPermissionOutcome::new(option),
                        ))
                    } else {
                        v2::RequestPermissionResponse::new(v2::RequestPermissionOutcome::Cancelled)
                    };
                    Ok(response)
                }
                Interaction::Elicitation(req, _) => Err(elicitation_answer(req, answer)?),
            };
            let interaction = guard.interaction.take().ok_or("Request expired")?;
            match interaction {
                Interaction::Permission(_, tx) => {
                    let response = validated.unwrap_or_else(|_| unreachable!());
                    tx.send(response)
                        .map_err(|_| "Request expired; wait for a new request")?;
                }
                Interaction::Elicitation(_, tx) => {
                    let response = validated.err().unwrap_or_else(|| unreachable!());
                    tx.send(response)
                        .map_err(|_| "Request expired; wait for a new request")?;
                }
            }
            Ok(json!({"answered":true}))
        }
        "detach" => {
            let (id, cwd, owner) = {
                let guard = state.lock().await;
                (guard.id.clone(), guard.cwd.clone(), guard.owner.clone())
            };
            if let (Some(id), Some(cwd)) = (id, cwd)
                && owner
                    .as_ref()
                    .and_then(|m| m.get("hive"))
                    .is_some_and(|h| h["role"] == "pilot")
            {
                resume(cx, state, id, cwd, "relinquish", true).await?;
            }
            *state.lock().await = ClientState::default();
            Ok(json!({"detached":true}))
        }
        _ => Err("Unknown bridge action".into()),
    }
}

async fn connection(url: String, mut commands: mpsc::UnboundedReceiver<Command>) {
    let state = Arc::new(Mutex::new(ClientState::default()));
    while !commands.is_closed() {
        let updates = state.clone();
        let permissions = state.clone();
        let elicitations = state.clone();
        let transport = match HttpClient::new(url.clone()) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!("ACP transport: {e}");
                return;
            }
        };
        let result = Client
            .v2()
            .on_receive_notification(
                async move |update: v2::UpdateSessionNotification, _: V2ConnectionTo<Agent>| {
                    updates.lock().await.accept_update(update);
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .on_receive_request(
                async move |req: v2::RequestPermissionRequest,
                            responder: Responder<v2::RequestPermissionResponse>,
                            _: V2ConnectionTo<Agent>| {
                    let (tx, rx) = oneshot::channel();
                    permissions.lock().await.interaction = Some(Interaction::Permission(req, tx));
                    responder.respond(rx.await.unwrap_or_else(|_| {
                        v2::RequestPermissionResponse::new(v2::RequestPermissionOutcome::Cancelled)
                    }))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: v2::CreateElicitationRequest,
                            responder: Responder<v2::CreateElicitationResponse>,
                            _: V2ConnectionTo<Agent>| {
                    let (tx, rx) = oneshot::channel();
                    elicitations.lock().await.interaction = Some(Interaction::Elicitation(req, tx));
                    responder.respond(rx.await.unwrap_or_else(|_| {
                        v2::CreateElicitationResponse::new(v2::ElicitationAction::Cancel)
                    }))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(transport, async |cx| {
                cx.send_request(
                    v2::InitializeRequest::new(
                        ProtocolVersion::V2,
                        v2::Implementation::new("hive-zellij", env!("CARGO_PKG_VERSION")),
                    )
                    .capabilities(
                        v2::ClientCapabilities::new().elicitation(
                            v2::ElicitationCapabilities::new()
                                .form(v2::ElicitationFormCapabilities::new())
                                .url(v2::ElicitationUrlCapabilities::new()),
                        ),
                    ),
                )
                .block_task()
                .await?;
                // Reconnect read-only so an intermittent network failure never steals pilot control.
                if let (Some(id), Some(cwd)) = {
                    let guard = state.lock().await;
                    (guard.id.clone(), guard.cwd.clone())
                } && let Err(error) = resume(&cx, &state, id, cwd, "observe", false).await
                {
                    tracing::warn!("Could not replay Zellij session: {error}");
                }
                loop {
                    tokio::select! {
                        _ = cx.incoming_closed() => break,
                        next = commands.recv() => match next {
                            Some(next) => {
                                // A prompt acknowledgement may be slow; polling and interaction replies must remain responsive.
                                let detach = next.request.action == "detach";
                                if detach {
                                    let result = command(&cx, &state, next.request).await;
                                    let _ = next.result.send(result);
                                    // Dropping this upstream connection removes observers as well as the pilot.
                                    break;
                                }
                                let cx = cx.clone();
                                let state = state.clone();
                                tokio::spawn(async move {
                                    let result = command(&cx, &state, next.request).await;
                                    let _ = next.result.send(result);
                                });
                            }
                            None => break,
                        },
                        else => break,
                    }
                }
                Ok(())
            })
            .await;
        if commands.is_closed() {
            break;
        }
        state.lock().await.owner = None;
        tracing::warn!("Zellij ACP connection lost ({result:?}); reconnecting");
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn bridge(
    State(server): State<Server>,
    Json(request): Json<Request>,
) -> (StatusCode, Json<Value>) {
    if request.action == "register" {
        let id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = mpsc::unbounded_channel();
        server.clients.lock().await.insert(
            id.clone(),
            Peer {
                commands: tx,
                last_seen: Instant::now(),
            },
        );
        tokio::spawn(connection(server.url.clone(), rx));
        return (StatusCode::OK, Json(json!({"client":id})));
    }
    let Some(id) = &request.client else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"Register first"})),
        );
    };
    let peer = {
        let mut clients = server.clients.lock().await;
        let Some(peer) = clients.get_mut(id) else {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error":"Bridge connection expired; reconnect"})),
            );
        };
        peer.last_seen = Instant::now();
        peer.commands.clone()
    };
    let (tx, rx) = oneshot::channel();
    if peer
        .send(Command {
            request,
            result: tx,
        })
        .is_err()
    {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error":"ACP client reconnecting"})),
        );
    }
    match tokio::time::timeout(Duration::from_secs(8), rx).await {
        Ok(Ok(Ok(value))) => (StatusCode::OK, Json(value)),
        Ok(Ok(Err(error))) => (StatusCode::CONFLICT, Json(json!({"error":error}))),
        _ => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error":"ACP request pending; refresh and retry safely"})),
        ),
    }
}

pub async fn serve(listen: SocketAddr, url: String) -> miette::Result<()> {
    if !listen.ip().is_loopback() {
        return Err(miette::miette!("Zellij bridge must bind to loopback"));
    }
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .map_err(|e| miette::miette!("Zellij bridge cannot bind {listen}: {e}"))?;
    let server = Server {
        url: url.replace("http://", "ws://"),
        clients: Arc::new(Mutex::new(HashMap::new())),
    };
    let clients = server.clients.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            // Dropping the command channel closes only the upstream ACP connection.
            clients
                .lock()
                .await
                .retain(|_, peer| peer.last_seen.elapsed() < Duration::from_secs(30));
        }
    });
    axum::serve(
        listener,
        Router::new()
            .route("/health", get(|| async { Json(json!({"ready":true})) }))
            .route("/bridge", post(bridge))
            .with_state(server),
    )
    .await
    .map_err(|e| miette::miette!("Zellij bridge failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::{Agent, Responder};
    use hive_daemon::config::{Config, Profile};
    use hive_protocol::AgentVersion;
    use std::collections::BTreeMap;

    #[test]
    fn a_transferred_pilot_loses_the_ability_to_mutate() {
        let mut state = ClientState {
            id: Some("hive-test".into()),
            generation: 2,
            ..ClientState::default()
        };
        let mut owner = v2::Meta::new();
        owner.insert(
            "hive".into(),
            json!({"role":"pilot","generation":2,"token":"secret"}),
        );
        state.owner = Some(owner);
        assert!(state.pilot());
        let mut event_meta = v2::Meta::new();
        event_meta.insert("hive".into(), json!({"seq":1}));
        state.accept_update(
            v2::UpdateSessionNotification::new(
                v2::SessionId::new("hive-test"),
                v2::SessionUpdate::Other(v2::OtherSessionUpdate::new(
                    "_hive_control",
                    BTreeMap::from([
                        ("generation".into(), json!(3)),
                        ("pilot".into(), json!(true)),
                    ]),
                )),
            )
            .meta(event_meta),
        );
        assert!(!state.pilot());
        assert!(state.owner.is_none());
    }

    #[test]
    fn polling_an_expired_cursor_reports_missing_history() {
        let mut state = ClientState {
            seq: 301,
            ..ClientState::default()
        };
        state.updates.push_back(json!({"seq":300,"update":{}}));
        assert!(state.snapshot(1).unwrap_err().contains("History expired"));
        assert_eq!(state.snapshot(300).unwrap()["updates"][0]["seq"], 300);
    }

    #[test]
    fn form_input_is_validated_and_converted_before_answering_the_agent() {
        let mut schema = v2::ElicitationSchema::new();
        schema.properties.insert(
            "approved".into(),
            serde_json::from_value(json!({"type":"boolean"})).unwrap(),
        );
        schema.required = Some(vec!["approved".into()]);
        let req = v2::CreateElicitationRequest::new(
            v2::ElicitationFormMode::new(v2::ElicitationSessionScope::new("hive-test"), schema),
            "Approve?",
        );
        assert!(elicitation_answer(&req, json!({"action":"accept","content":{}})).is_err());
        assert!(
            elicitation_answer(
                &req,
                json!({"action":"accept","content":{"approved":"maybe"}})
            )
            .is_err()
        );
        let answer = elicitation_answer(
            &req,
            json!({"action":"accept","content":{"approved":"true"}}),
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(answer).unwrap()["content"]["approved"],
            true
        );
        assert!(elicitation_answer(&req, json!({"action":"decline"})).is_ok());
    }

    #[tokio::test]
    async fn bridge_attaches_to_an_existing_acp_session_and_fences_its_old_pilot() {
        let agent_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", agent_listener.local_addr().unwrap());
        let mock = tokio::spawn(async move {
            let agent = || {
                Agent
                    .v2()
                    .name("mock")
                    .on_receive_request(
                        async |req: v2::InitializeRequest,
                               responder: Responder<v2::InitializeResponse>,
                               _: V2ConnectionTo<Client>| {
                            responder.respond(
                                v2::InitializeResponse::new(
                                    req.protocol_version,
                                    v2::Implementation::new("mock", "1"),
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
                            responder.respond(v2::NewSessionResponse::new(v2::SessionId::new(
                                "downstream",
                            )))
                        },
                        agent_client_protocol::on_receive_request!(),
                    )
                    .on_receive_request(
                        async |req: v2::PromptRequest,
                               responder: Responder<v2::PromptResponse>,
                               cx: V2ConnectionTo<Client>| {
                            responder
                                .respond(v2::PromptResponse::new(v2::MessageId::new("received")))?;
                            if req.prompt.iter().any(|block| matches!(block, v2::ContentBlock::Text(text) if text.text == "interact")) {
                                tokio::spawn(async move {
                                    let outcome: agent_client_protocol::Result<()> = async {
                                        let choice = cx.send_request(v2::RequestPermissionRequest::new(
                                            req.session_id.clone(), "Run tool", vec![v2::PermissionOption::new("allow", "Allow once", v2::PermissionOptionKind::AllowOnce)]))
                                            .block_task().await?;
                                        assert!(matches!(choice.outcome, v2::RequestPermissionOutcome::Selected(_)));
                                        let mut schema = v2::ElicitationSchema::new();
                                        schema.properties.insert("approved".into(), serde_json::from_value(json!({"type":"boolean"})).unwrap());
                                        schema.required = Some(vec!["approved".into()]);
                                        let answer = cx.send_request(v2::CreateElicitationRequest::new(
                                            v2::ElicitationFormMode::new(v2::ElicitationSessionScope::new(req.session_id.clone()),schema), "Approve?"))
                                            .block_task().await?;
                                        assert!(matches!(answer.action, v2::ElicitationAction::Accept(_)));
                                        let answer = cx.send_request(v2::CreateElicitationRequest::new(
                                            v2::ElicitationUrlMode::new(v2::ElicitationSessionScope::new(req.session_id.clone()),"auth", "https://example.com"), "Authenticate"))
                                            .block_task().await?;
                                        assert!(matches!(answer.action, v2::ElicitationAction::Accept(_)));
                                        cx.send_notification(v2::UpdateSessionNotification::new(req.session_id,
                                            v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(v2::IdleStateUpdate::new()))))
                                    }.await;
                                    outcome.unwrap();
                                });
                                return Ok(());
                            }
                            cx.send_notification(v2::UpdateSessionNotification::new(
                                req.session_id,
                                v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(
                                    v2::IdleStateUpdate::new(),
                                )),
                            ))
                        },
                        agent_client_protocol::on_receive_request!(),
                    )
            };
            axum::serve(
                agent_listener,
                agent_client_protocol_http::AcpHttpServer::new(agent).into_router(),
            )
            .await
            .unwrap();
        });
        let gateway_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let gateway_addr = gateway_listener.local_addr().unwrap();
        let mut config = Config {
            default_agent: Some("mock".into()),
            ..Config::default()
        };
        config.agents.insert(
            "mock".into(),
            Profile {
                name: None,
                command: None,
                args: vec![],
                env: BTreeMap::new(),
                endpoint: Some(endpoint),
                headers: BTreeMap::new(),
                versions: vec![AgentVersion::V2],
                pricing: Default::default(),
            },
        );
        let gateway = tokio::spawn(async move {
            axum::serve(
                gateway_listener,
                hive_daemon::router(hive_daemon::Broker::new(config)),
            )
            .await
            .unwrap();
        });
        let server = Server {
            url: format!("ws://{gateway_addr}"),
            clients: Arc::new(Mutex::new(HashMap::new())),
        };
        let bridge_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let bridge_addr = bridge_listener.local_addr().unwrap();
        let bridge_task = tokio::spawn(async move {
            axum::serve(
                bridge_listener,
                Router::new()
                    .route("/bridge", post(bridge))
                    .with_state(server),
            )
            .await
            .unwrap();
        });
        let http = reqwest::Client::new();
        let (opened_tx, opened_rx) = oneshot::channel();
        let (continue_tx, continue_rx) = oneshot::channel();
        let pilot = tokio::spawn(async move {
            Client
                .v2()
                .connect_with(
                    HttpClient::new(format!("ws://{gateway_addr}")).unwrap(),
                    async |cx| {
                        cx.send_request(v2::InitializeRequest::new(
                            ProtocolVersion::V2,
                            v2::Implementation::new("creator", "1"),
                        ))
                        .block_task()
                        .await?;
                        let cwd = std::env::current_dir().unwrap();
                        let opened = cx
                            .send_request(v2::NewSessionRequest::new(cwd))
                            .block_task()
                            .await?;
                        let id = opened.session_id.clone();
                        opened_tx.send((id.clone(), opened.meta.clone())).unwrap();
                        continue_rx.await.unwrap();
                        let mut meta = opened.meta.unwrap();
                        meta.get_mut("hive").unwrap()["key"] = json!("stale");
                        assert!(
                            cx.send_request(
                                v2::PromptRequest::new(id, vec!["old pilot".into()]).meta(meta)
                            )
                            .block_task()
                            .await
                            .is_err()
                        );
                        Ok(())
                    },
                )
                .await
                .unwrap();
        });
        let (id, _) = opened_rx.await.unwrap();
        let catalog: Value = http
            .get(format!("http://{gateway_addr}/hive/v1/sessions"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(catalog["sessions"][0]["sessionId"], id.to_string());
        let post = |value: Value| {
            http.post(format!("http://{bridge_addr}/bridge"))
                .json(&value)
                .send()
        };
        let registered: Value = post(json!({"action":"register"}))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let client = registered["client"].as_str().unwrap();
        let cwd = std::env::current_dir()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let observed: Value = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let response = post(
                    json!({"action":"attach","client":client,"sessionId":id.to_string(),"cwd":cwd}),
                )
                .await
                .unwrap();
                if response.status().is_success() {
                    break response.json().await.unwrap();
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(observed["pilot"], false);
        let controlled: Value =
            post(json!({"action":"takeover","client":client,"sessionId":id.to_string(),"cwd":cwd}))
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
        assert_eq!(controlled["pilot"], true);
        continue_tx.send(()).unwrap();
        pilot.await.unwrap();
        let result =
            post(json!({"action":"prompt","client":client,"text":"new pilot","key":"one"}))
                .await
                .unwrap();
        assert!(
            result.status().is_success(),
            "{}",
            result.text().await.unwrap()
        );
        // Wait for the first turn's idle event before starting a distinct prompt.
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let view: Value = post(json!({"action":"poll","client":client,"seq":1}))
                    .await
                    .unwrap()
                    .json()
                    .await
                    .unwrap();
                if view["running"] == false {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        let result = post(json!({"action":"prompt","client":client,"text":"interact","key":"two"}))
            .await
            .unwrap();
        let accepted = result.status().is_success();
        if !accepted {
            // A prompt may reach the agent before its acknowledgement is available.
            assert_eq!(result.status(), StatusCode::SERVICE_UNAVAILABLE);
            let error: Value = result.json().await.unwrap();
            assert!(error["error"].as_str().unwrap_or("").contains("pending"));
        }
        for (kind, answer) in [
            ("permission", json!({"optionId":"allow"})),
            (
                "elicitation",
                json!({"action":"accept","content":{"approved":"true"}}),
            ),
            ("elicitation", json!({"action":"accept"})),
        ] {
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    let view: Value = post(json!({"action":"poll","client":client,"seq":1}))
                        .await
                        .unwrap()
                        .json()
                        .await
                        .unwrap();
                    if view["interaction"]["type"] == kind
                        && (kind == "permission"
                            || (answer["content"].is_object()
                                && view["interaction"]["request"]["mode"] == "form")
                            || (kind == "elicitation"
                                && answer["content"].is_null()
                                && view["interaction"]["request"]["mode"] == "url"))
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap();
            let response = post(json!({"action":"answer","client":client,"answer":answer}))
                .await
                .unwrap();
            assert!(
                response.status().is_success(),
                "{}",
                response.text().await.unwrap()
            );
        }
        if !accepted {
            // Retrying the original key cannot dispatch a second agent prompt.
            let retried = post(json!({"action":"retry","client":client}))
                .await
                .unwrap();
            assert!(
                retried.status().is_success(),
                "{}",
                retried.text().await.unwrap()
            );
        }
        let detached: Value = post(json!({"action":"detach","client":client}))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(detached["detached"], true);
        let catalog: Value = http
            .get(format!("http://{gateway_addr}/hive/v1/sessions"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(catalog["sessions"][0]["sessionId"], id.to_string());
        assert!(catalog["sessions"][0]["controlOwner"].is_null());
        bridge_task.abort();
        gateway.abort();
        mock.abort();
    }
}
