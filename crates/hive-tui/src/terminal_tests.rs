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
        json!({"role":"pilot", "token":"fixture", "generation":1, "pilot":true, "nextSeq":seq + 1, "snapshotThroughSeq":seq,
            "activitySnapshot":{"version":1,"seq":seq,"foreground":"idle"}}),
    )])
}

fn choices() -> Vec<v2::SessionConfigOption> {
    vec![
        v2::SessionConfigOption::select(
            "model",
            "Model",
            "fast",
            vec![
                v2::SessionConfigSelectOption::new("fast", "Fast"),
                v2::SessionConfigSelectOption::new("careful", "Careful"),
            ],
        )
        .category(v2::SessionConfigOptionCategory::Model),
    ]
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
        "running" | "blocked" | "done" | "error" | "cancelled" => {
            let value = match action.as_str() {
                "running" => json!({"sessionUpdate":"state_update","state":"running"}),
                "blocked" => json!({"sessionUpdate":"state_update","state":"requires_action"}),
                "done" => json!({"sessionUpdate":"state_update","state":"idle","stopReason":"end_turn"}),
                "error" => json!({"sessionUpdate":"state_update","state":"idle","stopReason":"error","error":{"message":"fixture failure"}}),
                _ => json!({"sessionUpdate":"state_update","state":"idle","stopReason":"cancelled"}),
            };
            state.update(serde_json::from_value(value).unwrap());
        }
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
        "image" => state.update(serde_json::from_value(json!({"sessionUpdate":"agent_message", "messageId":"clipboard-image", "content":[crate::images::fixture_block()]})).unwrap()),
        _ => panic!("unknown fixture action"),
    }
    Json(json!({"ok":true}))
}

#[tokio::test]
#[ignore = "child fixture for tests/runtime.py; requires a controlling PTY"]
async fn runtime_fixture() {
    let state = Arc::new(Fixture::default());
    let agents = state.clone();
    let router = Router::new()
        .route("/fixture", get(report))
        .route("/fixture/{action}", post(action))
        .with_state(state)
        .merge(hive_protocol::AcpHttpServer::new(move || {
            let initialize = agents.clone();
            let resume = agents.clone();
            let prompt = agents.clone();
            let configuration = agents.clone();
            let create = agents.clone();
            let catalog = agents.clone();
            Agent.v2()
                .on_receive_request(async move |_: hive_protocol::remote::Initialize, responder: agent_client_protocol::Responder<serde_json::Value>, cx: V2ConnectionTo<Client>| {
                     *initialize.peer.lock().unwrap() = Some(cx);
                     responder.respond(json!({"protocolVersion":2,"info":{"name":"pty-fixture","version":"1"},"capabilities":{"session":{},"_meta":{"hive":{"version":1,"node":"node-11111111-1111-4111-8111-111111111111","features":{"agents":{},"selection":{},"control":{},"location":{},"peers":{},"profiles":{}}}}}}))
                 }, agent_client_protocol::on_receive_request!())
                .on_receive_request(async move |_: v2::ListSessionsRequest, responder: agent_client_protocol::Responder<v2::ListSessionsResponse>, _: V2ConnectionTo<Client>| {
                    catalog.catalogs.fetch_add(1, Ordering::SeqCst);
                    if catalog.stall.load(Ordering::SeqCst) {
                        let released = catalog.release.notified();
                        catalog.stalled.store(true, Ordering::SeqCst);
                        released.await;
                    }
                    let entry = |id: &str| json!({"sessionId":id,"cwd":std::env::current_dir().unwrap(),"_meta":{"hive":{"agent":"fixture","lifecycle":"active","activity":"idle","generation":1,"session":{"owner":"node-11111111-1111-4111-8111-111111111111","id":id}}}});
                    let mut sessions = vec![entry("hive-fixture")];
                    if std::env::var("HIVE_RUNTIME_STATUS").is_ok_and(|value| matches!(value.as_str(), "status" | "status-subset" | "status-on")) { sessions.push(entry("hive-second")); }
                    responder.respond(serde_json::from_value(json!({"sessions":sessions})).unwrap())
                }, agent_client_protocol::on_receive_request!())
                .on_receive_request(async |_: hive_protocol::remote::ListAgents, responder: agent_client_protocol::Responder<hive_protocol::remote::AgentsPage>, _: V2ConnectionTo<Client>| {
                    responder.respond(serde_json::from_value(json!({"agents":[{"id":"fixture","name":"Fixture","kind":"acp","transport":"stdio","versions":[],"isDefault":true,"capabilities":{},"launch":{"cwd":"required","workspace":"optional"}}],"defaultAgent":"fixture"})).unwrap())
                }, agent_client_protocol::on_receive_request!())
                .on_receive_request(async |_: hive_protocol::remote::ListProfiles, responder: agent_client_protocol::Responder<hive_protocol::remote::ProfilesPage>, _: V2ConnectionTo<Client>| {
                    responder.respond(hive_protocol::remote::ProfilesPage { profiles: Vec::new(), default_profile: None, next_cursor: None })
                }, agent_client_protocol::on_receive_request!())
                .on_receive_request(async |_: hive_protocol::remote::ListPeers, responder: agent_client_protocol::Responder<serde_json::Value>, _: V2ConnectionTo<Client>| { responder.respond(json!({"peers":[]})) }, agent_client_protocol::on_receive_request!())
                .on_receive_request(async |request: hive_protocol::remote::ResolveSession, responder: agent_client_protocol::Responder<hive_protocol::remote::SessionLocation>, _: V2ConnectionTo<Client>| {
                    responder.respond(serde_json::from_value(json!({"session":request.0["session"],"sessionId":request.0["session"]["id"],"cwd":std::env::current_dir().unwrap()})).unwrap())
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
    let clipboard = clipboard::Clipboard::worker(|| {
        |operation: &clipboard::Operation| match operation {
            clipboard::Operation::PrepareImage(mime, bytes) => Ok(Some(clipboard::Value::Image(
                draft::Attachment::new(mime.clone(), bytes.clone())?,
            ))),
            clipboard::Operation::ExportImage(image) => {
                Ok(Some(clipboard::Value::Image(image.attachment()?)))
            }
            _ => Err(clipboard::ClipboardError::Backend(
                "fixture clipboard unavailable".into(),
            )),
        }
    });
    run_with_clipboard(
        Options {
            owner: None,
            profile: None,
            terminal: capabilities::TerminalOptions {
                kitty_graphics: if std::env::var_os("HIVE_RUNTIME_RICH").is_some() {
                    capabilities::CapabilityOverride::Off
                } else {
                    capabilities::CapabilityOverride::Auto
                },
                osc5522_read: if std::env::var_os("HIVE_RUNTIME_RICH").is_some() {
                    capabilities::CapabilityOverride::On
                } else {
                    capabilities::CapabilityOverride::Auto
                },
                // Keep headless failure tests independent of the host's terminal and overrides.
                osc52_write: if std::env::var_os("HIVE_RUNTIME_OSC52").is_some() {
                    capabilities::CapabilityOverride::On
                } else {
                    capabilities::CapabilityOverride::Off
                },
                osc52_read: capabilities::CapabilityOverride::On,
                osc7501: match std::env::var("HIVE_RUNTIME_STATUS").as_deref() {
                    Ok("status-off") => capabilities::CapabilityOverride::Off,
                    Ok("status-on") => capabilities::CapabilityOverride::On,
                    _ => capabilities::CapabilityOverride::Auto,
                },
                ..Default::default()
            },
            cwd: None,
            project: None,
            workspace: None,
            workspace_id: None,
            base_ref: None,
            revision: None,
            branch: None,
            address: address.to_string(),
            transport: "ws".into(),
            agent: create.then(|| "fixture".into()),
            session: (!create).then(|| "hive-fixture".into()),
        },
        clipboard,
    )
    .await
    .unwrap();
    server.abort();
}

fn run_terminal_scenario(scenario: &str) {
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

// Each process owns its PTY, so independent scenarios need not share one timeout or execution slot.
#[test]
fn a_resumed_session_keeps_terminal_edits_unsent_until_enter() {
    run_terminal_scenario("resume");
}

#[test]
fn a_created_session_keeps_terminal_edits_unsent_until_enter() {
    run_terminal_scenario("create");
}

#[test]
fn osc52_clipboard_fallback_keeps_terminal_edits_unsent_until_enter() {
    run_terminal_scenario("osc52");
}

#[test]
fn rich_clipboard_content_keeps_terminal_edits_unsent_until_enter() {
    run_terminal_scenario("rich");
}

#[test]
fn terminal_status_negotiation_preserves_interactive_editing() {
    run_terminal_scenario("status");
}

#[test]
fn partial_terminal_status_support_preserves_interactive_editing() {
    run_terminal_scenario("status-subset");
}

#[test]
fn disabled_terminal_status_preserves_interactive_editing() {
    run_terminal_scenario("status-off");
}

#[test]
fn enabled_terminal_status_preserves_interactive_editing() {
    run_terminal_scenario("status-on");
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
