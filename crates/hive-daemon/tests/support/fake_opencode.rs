//! A scriptable stand-in for `opencode serve`, faithful to the routes and event shapes probed
//! against OpenCode 1.19.3 (see docs/acp-compatibility.md). It records every call so tests can
//! prove what Hive did *not* do (for example, create a second session).

#![allow(dead_code)]

use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    Json, Router,
    extract::{Path, Query, Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{
        Html, IntoResponse, Response,
        sse::{Event, Sse},
    },
    routing::{get, post},
};
use serde_json::{Value, json};
use tokio::sync::broadcast;
use tokio_stream::{StreamExt, wrappers::BroadcastStream};

/// Sentinel that makes every open SSE connection end, to simulate a dropped network.
const KILL: &str = "\u{0}kill";

pub struct Options {
    pub version: &'static str,
    pub v1: bool,
    pub v2: bool,
    /// Require HTTP basic auth with this password (user `opencode`).
    pub password: Option<String>,
    /// Answer unknown paths with `200 text/html`, as servers that proxy to a web UI do.
    pub html_fallback: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            version: "1.19.3",
            v1: true,
            v2: false,
            password: None,
            html_fallback: false,
        }
    }
}

#[derive(Clone)]
pub enum Script {
    /// Run a full turn that replies with this text.
    Reply(String),
    /// Go busy and stay busy until the test finishes the turn.
    Hold,
    /// Accept the prompt (204) and then do nothing at all.
    Silent,
}

#[derive(Clone)]
struct Session {
    id: String,
    title: String,
    directory: String,
}

struct Data {
    options: Options,
    sessions: Vec<Session>,
    /// v1 messages, `{info, parts}`, per session id.
    messages: Vec<(String, Value)>,
    busy: HashSet<String>,
    permissions: Vec<Value>,
    questions: Vec<Value>,
    calls: Vec<String>,
    prompts: Vec<String>,
    permission_replies: Vec<(String, Value)>,
    question_replies: Vec<(String, String, Value)>,
    script: Script,
    counter: u64,
    v2_log: Vec<Value>,
}

struct Inner {
    state: Mutex<Data>,
    v1_events: broadcast::Sender<String>,
    v2_events: broadcast::Sender<String>,
}

#[derive(Clone)]
pub struct Fake(Arc<Inner>);

pub async fn start(options: Options) -> (Fake, String) {
    let (v1_events, _) = broadcast::channel(1024);
    let (v2_events, _) = broadcast::channel(1024);
    let fake = Fake(Arc::new(Inner {
        state: Mutex::new(Data {
            options,
            sessions: Vec::new(),
            messages: Vec::new(),
            busy: HashSet::new(),
            permissions: Vec::new(),
            questions: Vec::new(),
            calls: Vec::new(),
            prompts: Vec::new(),
            permission_replies: Vec::new(),
            question_replies: Vec::new(),
            script: Script::Reply("done".into()),
            counter: 0,
            v2_log: Vec::new(),
        }),
        v1_events,
        v2_events,
    }));
    let router = Router::new()
        .route("/global/health", get(health))
        .route("/api/health", get(api_health))
        .route("/session", get(list_sessions).post(create_session))
        .route("/session/status", get(status))
        .route("/session/{id}", get(get_session))
        .route("/session/{id}/message", get(messages))
        .route("/session/{id}/prompt_async", post(prompt_async))
        .route("/session/{id}/abort", post(abort))
        .route("/event", get(event_stream))
        .route("/permission", get(list_permissions))
        .route("/permission/{id}/reply", post(reply_permission))
        .route("/question", get(list_questions))
        .route("/question/{id}/reply", post(reply_question))
        .route("/question/{id}/reject", post(reject_question))
        .route("/api/session", get(v2_list).post(v2_create))
        .route("/api/session/active", get(v2_active))
        .route("/api/session/{id}", get(v2_get))
        .route("/api/session/{id}/prompt", post(v2_prompt))
        .route("/api/session/{id}/interrupt", post(v2_interrupt))
        .route("/api/session/{id}/history", get(v2_history))
        .route("/api/session/{id}/event", get(v2_events_stream))
        .route("/api/session/{id}/permission", get(v2_permissions))
        .route(
            "/api/session/{id}/permission/{request}/reply",
            post(v2_reply_permission),
        )
        .fallback(fallback)
        .layer(middleware::from_fn_with_state(fake.clone(), auth))
        .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (fake, url)
}

// ---- test-facing controls -------------------------------------------------------------

impl Fake {
    fn state(&self) -> std::sync::MutexGuard<'_, Data> {
        self.0.state.lock().unwrap()
    }

    pub fn calls(&self) -> Vec<String> {
        self.state().calls.clone()
    }

    /// Number of recorded calls that start with `prefix`, e.g. `"POST /session"`.
    pub fn count(&self, prefix: &str) -> usize {
        self.state()
            .calls
            .iter()
            .filter(|call| call.starts_with(prefix))
            .count()
    }

    /// Number of recorded calls equal to `call`, e.g. exactly `"POST /session"` (session
    /// creation, not `POST /session/<id>/abort`).
    pub fn exact(&self, call: &str) -> usize {
        self.state().calls.iter().filter(|c| *c == call).count()
    }

    pub fn prompts(&self) -> Vec<String> {
        self.state().prompts.clone()
    }

    pub fn script(&self, script: Script) {
        self.state().script = script;
    }

    pub fn add_session(&self, id: &str, title: &str, directory: &str) {
        self.state().sessions.push(Session {
            id: id.into(),
            title: title.into(),
            directory: directory.into(),
        });
    }

    pub fn remove_session(&self, id: &str) {
        self.state().sessions.retain(|session| session.id != id);
    }

    /// Stores a finished user/assistant exchange (as if it happened in the OpenCode TUI).
    pub fn add_exchange(&self, session: &str, user: &str, assistant: &str) {
        let n = self.next();
        let mut state = self.state();
        state.messages.push((
            session.into(),
            message(
                session,
                &format!("msg_u{n}"),
                "user",
                &format!("prt_u{n}"),
                user,
            ),
        ));
        state.messages.push((
            session.into(),
            message(
                session,
                &format!("msg_a{n}"),
                "assistant",
                &format!("prt_a{n}"),
                assistant,
            ),
        ));
    }

    /// Rewrites a stored part's text without emitting any event: what a client missed while
    /// its stream was down.
    pub fn set_text(&self, part: &str, text: &str) {
        let mut state = self.state();
        for (_, message) in &mut state.messages {
            for p in message["parts"].as_array_mut().unwrap() {
                if p["id"] == part {
                    p["text"] = json!(text);
                }
            }
        }
    }

    pub fn emit(&self, event: Value) {
        let _ = self.0.v1_events.send(event.to_string());
    }

    /// Ends every open SSE connection; clients are expected to reconnect.
    pub fn kill_streams(&self) {
        let _ = self.0.v1_events.send(KILL.into());
        let _ = self.0.v2_events.send(KILL.into());
    }

    /// A turn started by another OpenCode client (the TUI).
    pub fn external_turn_start(&self, session: &str) {
        self.state().busy.insert(session.into());
        self.emit(status_event(session, "busy"));
    }

    pub fn external_turn_end(&self, session: &str) {
        self.state().busy.remove(session);
        self.emit(status_event(session, "idle"));
        self.emit(json!({"type": "session.idle", "properties": {"sessionID": session}}));
    }

    /// Completes a turn left open by `Script::Hold`.
    pub async fn finish_turn(&self, session: &str, reply: &str) {
        let n = self.next();
        self.reply(session, n, reply).await;
    }

    pub fn ask_permission(&self, session: &str, id: &str) {
        let request = json!({"id": id, "sessionID": session, "permission": "bash",
            "patterns": ["rm -rf build"], "metadata": {}, "always": []});
        self.state().permissions.push(request.clone());
        self.emit(json!({"type": "permission.asked", "properties": request}));
    }

    /// The TUI user answers first.
    pub fn answer_permission_natively(&self, session: &str, id: &str) {
        self.state().permissions.retain(|p| p["id"] != id);
        self.emit(json!({"type": "permission.replied", "properties": {
            "sessionID": session, "requestID": id, "reply": "once"}}));
    }

    pub fn permission_replies(&self) -> Vec<(String, Value)> {
        self.state().permission_replies.clone()
    }

    pub fn ask_question(&self, session: &str, id: &str, questions: Value) {
        let request = json!({"id": id, "sessionID": session, "questions": questions});
        self.state().questions.push(request.clone());
        self.emit(json!({"type": "question.asked", "properties": request}));
    }

    /// `(request id, "reply" | "reject", body)`.
    pub fn question_replies(&self) -> Vec<(String, String, Value)> {
        self.state().question_replies.clone()
    }

    fn next(&self) -> u64 {
        let mut state = self.state();
        state.counter += 1;
        state.counter
    }

    fn log(&self, call: String) {
        self.state().calls.push(call);
    }

    fn session(&self, id: &str) -> Option<Session> {
        self.state().sessions.iter().find(|s| s.id == id).cloned()
    }

    async fn pause(&self) {
        tokio::time::sleep(Duration::from_millis(15)).await;
    }

    /// The v1 turn the real server performs for a prompt, in the order observed on 1.19.3.
    async fn run_v1_turn(&self, session: &str, text: &str) {
        let n = self.next();
        let script = self.state().script.clone();
        if matches!(script, Script::Silent) {
            return;
        }
        let (user, part) = (format!("msg_u{n}"), format!("prt_u{n}"));
        self.state()
            .messages
            .push((session.into(), message(session, &user, "user", &part, text)));
        self.emit(json!({"type": "message.updated", "properties": {
            "sessionID": session, "info": {"id": user, "sessionID": session, "role": "user"}}}));
        self.emit(
            json!({"type": "message.part.updated", "properties": {"part": {
            "id": part, "sessionID": session, "messageID": user, "type": "text", "text": text}}}),
        );
        self.state().busy.insert(session.into());
        self.emit(status_event(session, "busy"));
        if let Script::Reply(reply) = script {
            self.pause().await;
            self.reply(session, n, &reply).await;
        }
    }

    async fn reply(&self, session: &str, n: u64, reply: &str) {
        let (assistant, part) = (format!("msg_a{n}"), format!("prt_a{n}"));
        self.emit(json!({"type": "message.updated", "properties": {
            "sessionID": session, "info": {"id": assistant, "sessionID": session, "role": "assistant"}}}));
        self.emit(json!({"type": "message.part.updated", "properties": {"part": {
            "id": part, "sessionID": session, "messageID": assistant, "type": "text", "text": ""}}}));
        for piece in reply.split_inclusive(' ') {
            self.pause().await;
            self.emit(json!({"type": "message.part.delta", "properties": {
                "sessionID": session, "messageID": assistant, "partID": part,
                "field": "text", "delta": piece}}));
        }
        self.state().messages.push((
            session.into(),
            message(session, &assistant, "assistant", &part, reply),
        ));
        self.emit(json!({"type": "message.part.updated", "properties": {"part": {
            "id": part, "sessionID": session, "messageID": assistant, "type": "text", "text": reply}}}));
        self.state().busy.remove(session);
        self.emit(status_event(session, "idle"));
        self.emit(json!({"type": "session.idle", "properties": {"sessionID": session}}));
    }

    fn v2_push(&self, event_type: &str, data: Value) {
        let mut state = self.state();
        let seq = state.v2_log.len() as u64 + 1;
        let event = json!({"id": format!("evt_{seq}"), "type": event_type,
            "durable": {"aggregateID": data["sessionID"], "seq": seq, "version": 1}, "data": data});
        state.v2_log.push(event.clone());
        drop(state);
        let _ = self.0.v2_events.send(event.to_string());
    }

    /// Seeds the v2 durable log as if the conversation happened in a v2 client.
    pub fn add_v2_exchange(&self, session: &str, user: &str, assistant: &str) {
        let n = self.next();
        self.v2_push(
            "session.next.prompted",
            json!({"sessionID": session, "messageID": format!("msg_u{n}"), "prompt": {"text": user}}),
        );
        self.v2_push(
            "session.next.text.ended",
            json!({"sessionID": session, "assistantMessageID": format!("msg_a{n}"),
                "textID": "t1", "text": assistant}),
        );
    }

    async fn run_v2_turn(&self, session: &str, text: &str) {
        let n = self.next();
        self.v2_push(
            "session.next.prompted",
            json!({"sessionID": session, "messageID": format!("msg_u{n}"), "prompt": {"text": text}}),
        );
        self.state().busy.insert(session.into());
        let assistant = format!("msg_a{n}");
        self.v2_push(
            "session.next.step.started",
            json!({"sessionID": session, "assistantMessageID": assistant}),
        );
        let Script::Reply(reply) = self.state().script.clone() else {
            return;
        };
        for piece in reply.split_inclusive(' ') {
            self.pause().await;
            self.v2_push(
                "session.next.text.delta",
                json!({"sessionID": session, "assistantMessageID": assistant,
                    "textID": "t1", "delta": piece}),
            );
        }
        self.v2_push(
            "session.next.text.ended",
            json!({"sessionID": session, "assistantMessageID": assistant,
                "textID": "t1", "text": reply}),
        );
        self.state().busy.remove(session);
        self.v2_push(
            "session.next.step.ended",
            json!({"sessionID": session, "assistantMessageID": assistant, "finish": "stop"}),
        );
    }
}

fn status_event(session: &str, kind: &str) -> Value {
    json!({"type": "session.status", "properties": {
        "sessionID": session, "status": {"type": kind}}})
}

fn message(session: &str, id: &str, role: &str, part: &str, text: &str) -> Value {
    json!({
        "info": {"id": id, "sessionID": session, "role": role},
        "parts": [{"id": part, "sessionID": session, "messageID": id, "type": "text", "text": text}],
    })
}

fn session_json(session: &Session) -> Value {
    json!({"id": session.id, "title": session.title, "directory": session.directory,
        "time": {"created": 1, "updated": 2}})
}

fn v2_session_json(session: &Session) -> Value {
    json!({"id": session.id, "title": session.title,
        "location": {"directory": session.directory}, "time": {"created": 1, "updated": 2}})
}

// ---- middleware and fallbacks ---------------------------------------------------------

fn base64(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | (*b as u32) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(TABLE[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

async fn auth(State(fake): State<Fake>, request: Request, next: Next) -> Response {
    let password = fake.state().options.password.clone();
    if let Some(password) = password {
        let expected = format!(
            "Basic {}",
            base64(format!("opencode:{password}").as_bytes())
        );
        let given = request
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok());
        if given != Some(expected.as_str()) {
            return (
                StatusCode::UNAUTHORIZED,
                [(header::WWW_AUTHENTICATE, "Basic realm=\"Secure Area\"")],
            )
                .into_response();
        }
    }
    next.run(request).await
}

async fn fallback(State(fake): State<Fake>) -> Response {
    if fake.state().options.html_fallback {
        Html("<!doctype html><title>OpenCode</title>").into_response()
    } else {
        StatusCode::NOT_FOUND.into_response()
    }
}

// ---- v1 routes ------------------------------------------------------------------------

async fn health(State(fake): State<Fake>) -> Response {
    let (v1, version) = {
        let state = fake.state();
        (state.options.v1, state.options.version)
    };
    if !v1 {
        return fallback(State(fake)).await;
    }
    Json(json!({"healthy": true, "version": version})).into_response()
}

async fn api_health(State(fake): State<Fake>) -> Response {
    if !fake.state().options.v2 {
        return fallback(State(fake)).await;
    }
    Json(json!({"healthy": true})).into_response()
}

#[derive(serde::Deserialize)]
struct Scope {
    directory: Option<String>,
}

async fn list_sessions(State(fake): State<Fake>, Query(scope): Query<Scope>) -> Json<Value> {
    fake.log("GET /session".into());
    let state = fake.state();
    Json(
        state
            .sessions
            .iter()
            .filter(|s| scope.directory.as_ref().is_none_or(|d| *d == s.directory))
            .map(session_json)
            .collect(),
    )
}

async fn create_session(State(fake): State<Fake>, Query(scope): Query<Scope>) -> Json<Value> {
    fake.log("POST /session".into());
    let n = fake.next();
    let session = Session {
        id: format!("ses_new{n}"),
        title: format!("New session - {n}"),
        directory: scope.directory.unwrap_or_default(),
    };
    fake.state().sessions.push(session.clone());
    Json(session_json(&session))
}

async fn status(State(fake): State<Fake>) -> Json<Value> {
    let state = fake.state();
    Json(
        state
            .busy
            .iter()
            .map(|id| (id.clone(), json!({"type": "busy"})))
            .collect::<serde_json::Map<_, _>>()
            .into(),
    )
}

async fn get_session(State(fake): State<Fake>, Path(id): Path<String>) -> Response {
    fake.log(format!("GET /session/{id}"));
    match fake.session(&id) {
        Some(session) => Json(session_json(&session)).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn messages(State(fake): State<Fake>, Path(id): Path<String>) -> Response {
    fake.log(format!("GET /session/{id}/message"));
    if fake.session(&id).is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    let state = fake.state();
    Json(
        state
            .messages
            .iter()
            .filter(|(session, _)| *session == id)
            .map(|(_, message)| message.clone())
            .collect::<Vec<_>>(),
    )
    .into_response()
}

async fn prompt_async(
    State(fake): State<Fake>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> StatusCode {
    fake.log(format!("POST /session/{id}/prompt_async"));
    let text = body["parts"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    fake.state().prompts.push(text.clone());
    let fake = fake.clone();
    tokio::spawn(async move { fake.run_v1_turn(&id, &text).await });
    StatusCode::NO_CONTENT
}

async fn abort(State(fake): State<Fake>, Path(id): Path<String>) -> Json<bool> {
    fake.log(format!("POST /session/{id}/abort"));
    fake.state().busy.remove(&id);
    fake.emit(status_event(&id, "idle"));
    fake.emit(json!({"type": "session.idle", "properties": {"sessionID": id}}));
    Json(true)
}

async fn event_stream(State(fake): State<Fake>) -> Response {
    fake.log("GET /event".into());
    let live = BroadcastStream::new(fake.0.v1_events.subscribe());
    sse(tokio_stream::once(Ok(
        json!({"type": "server.connected", "properties": {}}).to_string()
    ))
    .chain(live))
}

fn sse(
    stream: impl tokio_stream::Stream<
        Item = Result<String, tokio_stream::wrappers::errors::BroadcastStreamRecvError>,
    > + Send
    + 'static,
) -> Response {
    Sse::new(
        stream
            .take_while(|item| !matches!(item, Ok(data) if data == KILL))
            .filter_map(|item| item.ok())
            .map(|data| Ok::<Event, std::convert::Infallible>(Event::default().data(data))),
    )
    .into_response()
}

async fn list_permissions(State(fake): State<Fake>) -> Json<Value> {
    Json(Value::Array(fake.state().permissions.clone()))
}

async fn reply_permission(
    State(fake): State<Fake>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Json<bool> {
    fake.log(format!("POST /permission/{id}/reply"));
    let mut state = fake.state();
    state.permission_replies.push((id.clone(), body));
    state.permissions.retain(|p| p["id"] != id);
    Json(true)
}

async fn list_questions(State(fake): State<Fake>) -> Json<Value> {
    Json(Value::Array(fake.state().questions.clone()))
}

async fn reply_question(
    State(fake): State<Fake>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Json<bool> {
    fake.log(format!("POST /question/{id}/reply"));
    let mut state = fake.state();
    state
        .question_replies
        .push((id.clone(), "reply".into(), body));
    state.questions.retain(|q| q["id"] != id);
    Json(true)
}

async fn reject_question(State(fake): State<Fake>, Path(id): Path<String>) -> Json<bool> {
    fake.log(format!("POST /question/{id}/reject"));
    let mut state = fake.state();
    state
        .question_replies
        .push((id.clone(), "reject".into(), Value::Null));
    state.questions.retain(|q| q["id"] != id);
    Json(true)
}

// ---- v2 preview routes ----------------------------------------------------------------

async fn v2_list(State(fake): State<Fake>) -> Json<Value> {
    fake.log("GET /api/session".into());
    let state = fake.state();
    Json(json!({"data": state.sessions.iter().map(v2_session_json).collect::<Vec<_>>()}))
}

async fn v2_create(State(fake): State<Fake>, Json(body): Json<Value>) -> Json<Value> {
    fake.log("POST /api/session".into());
    let n = fake.next();
    let session = Session {
        id: format!("ses_v2new{n}"),
        title: format!("New session - {n}"),
        directory: body["location"]["directory"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
    };
    fake.state().sessions.push(session.clone());
    Json(json!({"data": v2_session_json(&session)}))
}

async fn v2_active(State(fake): State<Fake>) -> Json<Value> {
    let state = fake.state();
    let active: serde_json::Map<_, _> = state
        .busy
        .iter()
        .map(|id| (id.clone(), json!({"type": "running"})))
        .collect();
    Json(json!({"data": active}))
}

async fn v2_get(State(fake): State<Fake>, Path(id): Path<String>) -> Response {
    fake.log(format!("GET /api/session/{id}"));
    match fake.session(&id) {
        Some(session) => Json(json!({"data": v2_session_json(&session)})).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn v2_prompt(
    State(fake): State<Fake>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Json<Value> {
    fake.log(format!("POST /api/session/{id}/prompt"));
    let text = body["prompt"]["text"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    fake.state().prompts.push(text.clone());
    let runner = fake.clone();
    let session = id.clone();
    tokio::spawn(async move { runner.run_v2_turn(&session, &text).await });
    Json(json!({"data": {"admittedSeq": 1, "id": "msg_x", "sessionID": id}}))
}

async fn v2_interrupt(State(fake): State<Fake>, Path(id): Path<String>) -> StatusCode {
    fake.log(format!("POST /api/session/{id}/interrupt"));
    fake.state().busy.remove(&id);
    StatusCode::NO_CONTENT
}

#[derive(serde::Deserialize)]
struct After {
    after: Option<u64>,
    limit: Option<usize>,
}

async fn v2_history(
    State(fake): State<Fake>,
    Path(id): Path<String>,
    Query(after): Query<After>,
) -> Json<Value> {
    fake.log(format!("GET /api/session/{id}/history"));
    let limit = after.limit.unwrap_or(usize::MAX);
    let after = after.after.unwrap_or(0);
    let state = fake.state();
    let data: Vec<_> = state
        .v2_log
        .iter()
        .filter(|e| e["durable"]["seq"].as_u64().unwrap_or(0) > after)
        .cloned()
        .collect();
    let has_more = data.len() > limit;
    Json(json!({"data": &data[..data.len().min(limit)], "hasMore": has_more}))
}

async fn v2_events_stream(
    State(fake): State<Fake>,
    Path(id): Path<String>,
    Query(after): Query<After>,
) -> Response {
    fake.log(format!(
        "GET /api/session/{id}/event after={}",
        after.after.unwrap_or(0)
    ));
    // Subscribe before reading the log so nothing falls between replay and live.
    let live = fake.0.v2_events.subscribe();
    let mut seen = after.after.unwrap_or(0);
    let replay: Vec<_> = fake
        .state()
        .v2_log
        .iter()
        .filter(|e| e["durable"]["seq"].as_u64().unwrap_or(0) > seen)
        .map(|e| Ok(e.to_string()))
        .collect();
    if let Some(Ok(last)) = replay.last() {
        seen = serde_json::from_str::<Value>(last).unwrap()["durable"]["seq"]
            .as_u64()
            .unwrap();
    }
    sse(
        tokio_stream::iter(replay).chain(BroadcastStream::new(live).filter(move |item| {
            // Drop live events the replay already delivered.
            match item {
                Ok(data) if data != KILL => serde_json::from_str::<Value>(data)
                    .ok()
                    .and_then(|e| e["durable"]["seq"].as_u64())
                    .is_none_or(|seq| seq > seen),
                _ => true,
            }
        })),
    )
}

async fn v2_permissions(State(_fake): State<Fake>, _headers: HeaderMap) -> Json<Value> {
    Json(json!({"data": []}))
}

async fn v2_reply_permission(
    State(fake): State<Fake>,
    Path((id, request)): Path<(String, String)>,
) -> StatusCode {
    fake.log(format!("POST /api/session/{id}/permission/{request}/reply"));
    StatusCode::NO_CONTENT
}
