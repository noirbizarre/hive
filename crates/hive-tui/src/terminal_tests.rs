//! Run the shipped event loop in a controlling PTY, including in instrumented CI test binaries.

use super::*;
use axum::{
    Json, Router,
    extract::{Path, State},
    routing::{get, post},
};
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

#[derive(Default)]
struct Fixture {
    peer: Mutex<Option<V2ConnectionTo<Client>>>,
    prompts: Mutex<Vec<Value>>,
    seq: AtomicU64,
    resumes: AtomicU64,
    permissions: AtomicU64,
    configurations: AtomicU64,
    catalogs: AtomicU64,
    creations: AtomicU64,
    stall: AtomicBool,
    stalled: AtomicBool,
    release: tokio::sync::Notify,
}

fn owner(seq: u64) -> v2::Meta {
    v2::Meta::from_iter([(
        "hive".into(),
        json!({"role":"pilot", "token":"fixture", "generation":1, "pilot":true, "nextSeq":seq + 1, "snapshotThroughSeq":seq}),
    )])
}

fn choices() -> Vec<v2::SessionConfigOption> {
    vec![v2::SessionConfigOption::select(
        "model",
        "Model",
        "fast",
        vec![
            v2::SessionConfigSelectOption::new("fast", "Fast"),
            v2::SessionConfigSelectOption::new("careful", "Careful"),
        ],
    )]
}

impl Fixture {
    fn update(&self, update: v2::SessionUpdate) {
        let seq = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        self.peer
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .send_notification(
                v2::UpdateSessionNotification::new("hive-fixture", update)
                    .meta(v2::Meta::from_iter([("hive".into(), json!({"seq":seq}))])),
            )
            .unwrap();
    }
}

async fn report(State(state): State<Arc<Fixture>>) -> Json<Value> {
    Json(
        json!({"prompts":*state.prompts.lock().unwrap(), "resumes":state.resumes.load(Ordering::SeqCst),
        "permissions":state.permissions.load(Ordering::SeqCst), "configurations":state.configurations.load(Ordering::SeqCst),
        "catalogs":state.catalogs.load(Ordering::SeqCst), "creations":state.creations.load(Ordering::SeqCst), "stalled":state.stalled.load(Ordering::SeqCst)}),
    )
}

async fn action(State(state): State<Arc<Fixture>>, Path(action): Path<String>) -> Json<Value> {
    match action.as_str() {
        "update" => state.update(v2::SessionUpdate::AgentMessageChunk(v2::ContentChunk::new(
            (0..40)
                .map(|index| format!("History row {index}\n"))
                .collect::<String>()
                .into(),
            "stream",
        ))),
        "gap" => {
            state.seq.fetch_add(1, Ordering::SeqCst);
            state.update(v2::SessionUpdate::AgentMessageChunk(v2::ContentChunk::new(
                "gap".into(),
                "gap",
            )));
        }
        "stall" => {
            state.stall.store(true, Ordering::SeqCst);
        }
        "release" => {
            state.stall.store(false, Ordering::SeqCst);
            state.release.notify_waiters();
        }
        "permission" => {
            let peer = state.peer.lock().unwrap().as_ref().unwrap().clone();
            tokio::spawn(async move {
                peer.send_request(v2::RequestPermissionRequest::new(
                    "hive-fixture",
                    "PTY approval",
                    vec![v2::PermissionOption::new(
                        "allow",
                        "Allow",
                        v2::PermissionOptionKind::AllowOnce,
                    )],
                ))
                .block_task()
                .await
                .unwrap();
                state.permissions.fetch_add(1, Ordering::SeqCst);
            });
        }
        _ => panic!("unknown fixture action"),
    }
    Json(json!({"ok":true}))
}

#[tokio::test]
#[ignore = "child fixture for tests/runtime.py; requires a controlling PTY"]
async fn runtime_fixture() {
    let state = Arc::new(Fixture::default());
    let sessions = state.clone();
    let providers = state.clone();
    let agents = state.clone();
    let router = Router::new()
        .route("/fixture", get(report))
        .route("/fixture/{action}", post(action))
        .route("/v1/sessions", get(async move || {
            sessions.catalogs.fetch_add(1, Ordering::SeqCst);
            if sessions.stall.load(Ordering::SeqCst) {
                // Register before publishing readiness so the test cannot lose a wake-up.
                let released = sessions.release.notified();
                sessions.stalled.store(true, Ordering::SeqCst);
                released.await;
            }
            Json(json!({"sessions":[{"sessionId":"hive-fixture", "cwd":std::env::current_dir().unwrap(), "agent":"fixture", "lifecycle":"active", "activity":"idle", "generation":1}]}))
        }))
        .route("/v1/providers", get(async move || {
            providers.catalogs.fetch_add(1, Ordering::SeqCst);
            Json(json!({"agents":[{"id":"fixture"}]}))
        }))
        .with_state(state)
        .merge(hive_protocol::AcpHttpServer::new(move || {
            let initialize = agents.clone();
            let resume = agents.clone();
            let prompt = agents.clone();
            let configuration = agents.clone();
            let create = agents.clone();
            Agent.v2()
                .on_receive_request(async move |request: v2::InitializeRequest, responder: agent_client_protocol::Responder<v2::InitializeResponse>, cx: V2ConnectionTo<Client>| {
                    *initialize.peer.lock().unwrap() = Some(cx);
                    responder.respond(v2::InitializeResponse::new(request.protocol_version, v2::Implementation::new("pty-fixture", "1")))
                }, agent_client_protocol::on_receive_request!())
                .on_receive_request(async move |request: v2::ResumeSessionRequest, responder: agent_client_protocol::Responder<v2::ResumeSessionResponse>, _: V2ConnectionTo<Client>| {
                    resume.resumes.fetch_add(1, Ordering::SeqCst);
                    let mut meta = owner(resume.seq.load(Ordering::SeqCst));
                    if request.meta.as_ref().is_some_and(|meta| matches!(meta["hive"]["mode"].as_str(), Some("observe" | "relinquish"))) {
                        meta.get_mut("hive").unwrap()["role"] = "observer".into();
                    }
                    responder.respond(v2::ResumeSessionResponse::new().config_options(choices()).meta(meta))
                }, agent_client_protocol::on_receive_request!())
                .on_receive_request(async move |_: v2::NewSessionRequest, responder: agent_client_protocol::Responder<v2::NewSessionResponse>, _: V2ConnectionTo<Client>| {
                    create.creations.fetch_add(1, Ordering::SeqCst);
                    responder.respond(v2::NewSessionResponse::new("hive-fixture").config_options(choices()).meta(owner(0)))
                }, agent_client_protocol::on_receive_request!())
                .on_receive_request(async move |request: v2::PromptRequest, responder: agent_client_protocol::Responder<v2::PromptResponse>, _: V2ConnectionTo<Client>| {
                    prompt.prompts.lock().unwrap().push(serde_json::to_value(&request.prompt).unwrap());
                    prompt.update(v2::SessionUpdate::UserMessage(v2::UserMessage::new("user").content(request.prompt)));
                    responder.respond(v2::PromptResponse::new("ack"))
                }, agent_client_protocol::on_receive_request!())
                .on_receive_request(async move |_: v2::SetSessionConfigOptionRequest, responder: agent_client_protocol::Responder<v2::SetSessionConfigOptionResponse>, _: V2ConnectionTo<Client>| {
                    configuration.configurations.fetch_add(1, Ordering::SeqCst);
                    responder.respond(v2::SetSessionConfigOptionResponse::new(choices()))
                }, agent_client_protocol::on_receive_request!())
                .on_receive_notification(async |_: v2::CancelSessionNotification, _: V2ConnectionTo<Client>| Ok(()), agent_client_protocol::on_receive_notification!())
        }).into_router());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    eprintln!("HIVE_RUNTIME_READY http://{address}");
    let create = std::env::var_os("HIVE_RUNTIME_CREATE").is_some();
    run(Options {
        address: address.to_string(),
        transport: "ws".into(),
        agent: create.then(|| "fixture".into()),
        session: (!create).then(|| "hive-fixture".into()),
    })
    .await
    .unwrap();
    server.abort();
}

#[test]
fn the_real_event_loop_keeps_paste_and_external_edits_unsent_until_enter() {
    for scenario in ["resume", "create"] {
        // Reuse this instrumented executable so child-process execution contributes to coverage too.
        let output = std::process::Command::new("python3")
            .arg("-B")
            .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/runtime.py"))
            .arg(std::env::current_exe().unwrap())
            .arg(scenario)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn terminal_handoff_and_bracketed_paste_restore_real_tty_modes() {
    let output = std::process::Command::new("python3")
        .arg("-B")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/terminal.py"))
        .arg(std::env::current_exe().unwrap())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
