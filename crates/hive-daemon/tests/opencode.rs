//! Adoption of OpenCode server sessions, against a scripted fake of `opencode serve`.
//!
//! The fake records every call, so the central guarantee (adopting never creates a second
//! conversation) is asserted by counting `POST /session`, not by inference.

#[path = "support/fake_opencode.rs"]
mod fake_opencode;

use std::{
    collections::BTreeMap,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use agent_client_protocol::{
    Agent, Client, Error, Responder, V2ConnectionTo,
    schema::{ProtocolVersion, v2},
};
use agent_client_protocol_http::HttpClient;
use fake_opencode::{Options, Script};
use hive_daemon::config::{Config, Profile, ProfileKind};
use hive_protocol::AgentVersion;
use serde_json::{Value, json};
use tokio::{sync::mpsc, time::timeout};

type Updates = mpsc::UnboundedReceiver<v2::UpdateSessionNotification>;

fn cwd() -> String {
    std::env::current_dir().unwrap().display().to_string()
}

fn opencode_profile(url: &str, password_env: Option<&str>) -> Profile {
    Profile {
        name: None,
        kind: ProfileKind::OpencodeServer,
        command: None,
        args: vec![],
        env: BTreeMap::new(),
        endpoint: Some(url.into()),
        headers: BTreeMap::new(),
        versions: vec![AgentVersion::V2],
        username: None,
        password_env: password_env.map(str::to_owned),
        pricing: Default::default(),
    }
}

fn config(url: &str) -> Config {
    config_with(opencode_profile(url, None))
}

fn config_with(profile: Profile) -> Config {
    Config {
        default_agent: Some("oc".into()),
        agents: BTreeMap::from([("oc".into(), profile)]),
    }
}

async fn serve(broker: hive_daemon::Broker) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = hive_daemon::router(broker);
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    address
}

async fn start(config: Config) -> (hive_daemon::Broker, SocketAddr) {
    let broker = hive_daemon::Broker::new(config);
    let address = serve(broker.clone()).await;
    (broker, address)
}

async fn catalog(address: SocketAddr) -> Value {
    reqwest::get(format!("http://{address}/v1/sessions"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

/// Polls the catalog until `done` holds, so tests wait on state instead of sleeping.
async fn wait_for(address: SocketAddr, done: impl Fn(&Value) -> bool) -> Value {
    timeout(Duration::from_secs(15), async {
        loop {
            let snapshot = catalog(address).await;
            if done(&snapshot) {
                return snapshot;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the catalog never reached the expected state")
}

async fn discover(address: SocketAddr, agent: &str, directory: Option<&str>) -> (u16, Value) {
    let mut url = reqwest::Url::parse(&format!(
        "http://{address}/v1/providers/{agent}/native-sessions"
    ))
    .unwrap();
    if let Some(directory) = directory {
        url.query_pairs_mut().append_pair("directory", directory);
    }
    let response = reqwest::get(url).await.unwrap();
    (response.status().as_u16(), response.json().await.unwrap())
}

/// What the test's "client" does when Hive, as pilot, forwards a permission or form.
#[derive(Clone, Default)]
struct Pilot {
    /// Option ID to select, or `None` to hold the request open.
    permission: Option<&'static str>,
    form: Option<BTreeMap<String, v2::ElicitationContentValue>>,
    seen: Arc<Mutex<Vec<Value>>>,
    held: Arc<Mutex<Vec<Responder<v2::RequestPermissionResponse>>>>,
}

impl Pilot {
    /// Answers every held permission request now.
    fn release(&self, option: &str) {
        for responder in self.held.lock().unwrap().drain(..) {
            let _ = responder.respond(v2::RequestPermissionResponse::new(
                v2::RequestPermissionOutcome::Selected(v2::SelectedPermissionOutcome::new(
                    option.to_owned(),
                )),
            ));
        }
    }
}

async fn connect<T>(
    address: SocketAddr,
    pilot: Pilot,
    body: impl AsyncFnOnce(V2ConnectionTo<Agent>, Updates) -> agent_client_protocol::Result<T>,
) -> T {
    let (tx, rx) = mpsc::unbounded_channel();
    let permissions = pilot.clone();
    let forms = pilot.clone();
    timeout(
        Duration::from_secs(30),
        Client
            .v2()
            .on_receive_notification(
                async move |update: v2::UpdateSessionNotification, _: V2ConnectionTo<Agent>| {
                    tx.send(update).map_err(Error::into_internal_error)
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .on_receive_request(
                async move |request: v2::RequestPermissionRequest,
                            responder: Responder<v2::RequestPermissionResponse>,
                            _: V2ConnectionTo<Agent>| {
                    permissions
                        .seen
                        .lock()
                        .unwrap()
                        .push(serde_json::to_value(&request).unwrap());
                    match permissions.permission {
                        Some(option) => responder.respond(v2::RequestPermissionResponse::new(
                            v2::RequestPermissionOutcome::Selected(
                                v2::SelectedPermissionOutcome::new(option),
                            ),
                        )),
                        None => {
                            permissions.held.lock().unwrap().push(responder);
                            Ok(())
                        }
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |request: v2::CreateElicitationRequest,
                            responder: Responder<v2::CreateElicitationResponse>,
                            _: V2ConnectionTo<Agent>| {
                    forms
                        .seen
                        .lock()
                        .unwrap()
                        .push(serde_json::to_value(&request).unwrap());
                    let mut accept = v2::ElicitationAcceptAction::new();
                    accept.content = forms.form.clone();
                    responder.respond(v2::CreateElicitationResponse::new(
                        v2::ElicitationAction::Accept(accept),
                    ))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(
                HttpClient::new(format!("ws://{address}")).unwrap(),
                async move |cx| {
                    cx.send_request(
                        v2::InitializeRequest::new(
                            ProtocolVersion::V2,
                            v2::Implementation::new("opencode-test", "1"),
                        )
                        .capabilities(
                            v2::ClientCapabilities::new().elicitation(
                                v2::ElicitationCapabilities::new()
                                    .form(v2::ElicitationFormCapabilities::new()),
                            ),
                        ),
                    )
                    .block_task()
                    .await?;
                    body(cx, rx).await
                },
            ),
    )
    .await
    .expect("the client scenario timed out")
    .expect("the client scenario failed")
}

fn adopt_request(native_id: &str) -> v2::NewSessionRequest {
    let mut meta = v2::Meta::new();
    meta.insert(
        "hive".into(),
        json!({"agent": "oc", "adopt": {"nativeId": native_id}}),
    );
    v2::NewSessionRequest::new(std::env::current_dir().unwrap()).meta(meta)
}

fn new_request() -> v2::NewSessionRequest {
    v2::NewSessionRequest::new(std::env::current_dir().unwrap())
}

fn controlled(meta: &Option<v2::Meta>, key: &str) -> v2::Meta {
    let mut value = meta.clone().unwrap();
    value.get_mut("hive").unwrap()["key"] = json!(key);
    value
}

fn prompt(id: &v2::SessionId, meta: &Option<v2::Meta>, key: &str, text: &str) -> v2::PromptRequest {
    v2::PromptRequest::new(id.clone(), vec![text.into()]).meta(controlled(meta, key))
}

/// Receives updates for `id` until `stop` matches one, returning everything seen.
async fn until(
    updates: &mut Updates,
    id: &v2::SessionId,
    stop: impl Fn(&v2::SessionUpdate) -> bool,
) -> Vec<v2::SessionUpdate> {
    let mut seen = Vec::new();
    timeout(Duration::from_secs(15), async {
        while let Some(update) = updates.recv().await {
            if &update.session_id != id {
                continue;
            }
            let done = stop(&update.update);
            seen.push(update.update);
            if done {
                break;
            }
        }
    })
    .await
    .expect("the expected update never arrived");
    seen
}

fn is_idle(update: &v2::SessionUpdate) -> bool {
    matches!(
        update,
        v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(_))
    )
}

fn is_running(update: &v2::SessionUpdate) -> bool {
    matches!(
        update,
        v2::SessionUpdate::StateUpdate(v2::StateUpdate::Running(_))
    )
}

fn agent_text(updates: &[v2::SessionUpdate]) -> String {
    updates
        .iter()
        .filter_map(|update| match update {
            v2::SessionUpdate::AgentMessage(message) => serde_json::to_value(&message.content)
                .ok()?[0]["text"]
                .as_str()
                .map(str::to_owned),
            v2::SessionUpdate::AgentMessageChunk(chunk) => serde_json::to_value(&chunk.content)
                .ok()?["text"]
                .as_str()
                .map(str::to_owned),
            _ => None,
        })
        .collect()
}

fn user_texts(updates: &[v2::SessionUpdate]) -> Vec<String> {
    updates
        .iter()
        .filter_map(|update| match update {
            v2::SessionUpdate::UserMessage(message) => serde_json::to_value(&message.content)
                .ok()?[0]["text"]
                .as_str()
                .map(str::to_owned),
            _ => None,
        })
        .collect()
}

/// Drains whatever replay currently holds for `id`.
async fn replay(address: SocketAddr, id: &v2::SessionId) -> Vec<v2::SessionUpdate> {
    let id = id.clone();
    connect(address, Pilot::default(), async move |cx, mut updates| {
        let mut meta = v2::Meta::new();
        meta.insert("hive".into(), json!({"mode": "observe"}));
        cx.send_request(
            v2::ResumeSessionRequest::new(id.clone(), std::env::current_dir().unwrap())
                .replay_from(v2::ReplayFrom::Start(v2::ReplayFromStart::new()))
                .meta(meta),
        )
        .block_task()
        .await?;
        tokio::time::sleep(Duration::from_millis(200)).await;
        let mut seen = Vec::new();
        while let Ok(update) = updates.try_recv() {
            if update.session_id == id {
                seen.push(update.update);
            }
        }
        Ok(seen)
    })
    .await
}

async fn adopt(address: SocketAddr, native_id: &str) -> v2::SessionId {
    let native_id = native_id.to_owned();
    connect(address, Pilot::default(), async move |cx, _| {
        Ok(cx
            .send_request(adopt_request(&native_id))
            .block_task()
            .await?
            .session_id)
    })
    .await
}

fn data_code(error: &Error) -> String {
    error.data.as_ref().expect("structured error data")["code"]
        .as_str()
        .expect("a diagnostic code")
        .to_owned()
}

// ---- discovery and adoption ---------------------------------------------------------------

#[test]
fn reload_keeps_native_sessions_on_their_original_server_until_explicit_close() {
    let dir = std::env::temp_dir().join(format!("hive-oc-reload-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&dir).unwrap();
    {
        // Axum's upgraded connections can outlive its listener. Drop the runtime before
        // removing SQLite files so Windows also observes all background handles closing.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
    let (first, first_url) = fake_opencode::start(Options::default()).await;
    let (_second, second_url) = fake_opencode::start(Options::default()).await;
    first.add_session("ses_a", "Original", &cwd());
    let path = dir.join("config.toml");
    let text = |url: &str| {
        format!("default_agent = 'oc'\n[agents.oc]\nkind = 'opencode-server'\nendpoint = {url:?}\n")
    };
    std::fs::write(&path, text(&first_url)).unwrap();
    let source = hive_daemon::config::ConfigSource::new(Some(path.clone())).unwrap();
    let broker =
        hive_daemon::Broker::persistent(source.load().unwrap(), dir.join("state/sessions.sqlite3"))
            .unwrap()
            .with_config_source(source);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = hive_daemon::router(broker.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    connect(address, Pilot::default(), async |cx, mut updates| {
        let session = cx.send_request(adopt_request("ses_a")).block_task().await?;
        std::fs::write(&path, text(&second_url)).unwrap();
        assert!(matches!(
            broker.reload_config().await,
            Err(hive_daemon::reload::ReloadError::Identity(_))
        ));
        std::fs::write(&path, "").unwrap();
        broker.reload_config().await.unwrap();
        std::fs::write(&path, text(&second_url)).unwrap();
        assert!(matches!(
            broker.reload_config().await,
            Err(hive_daemon::reload::ReloadError::Identity(_))
        ));
        cx.send_request(prompt(
            &session.session_id,
            &session.meta,
            "after-reload",
            "continue",
        ))
        .block_task()
        .await?;
        until(&mut updates, &session.session_id, is_idle).await;
        cx.send_request(v2::CloseSessionRequest::new(session.session_id).meta(session.meta))
            .block_task()
            .await?;
        broker.reload_config().await.unwrap();
        assert_eq!(discover(address, "oc", None).await.0, 200);
        Ok(())
    })
    .await;
    broker.shutdown().await;
    server.abort();
    let _ = server.await;
    drop(broker);
    });
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn discovery_lists_native_sessions_and_marks_the_adopted_ones() {
    let (fake, url) = fake_opencode::start(Options::default()).await;
    fake.add_session("ses_a", "Fix the bug", &cwd());
    fake.add_session("ses_b", "Elsewhere", "/some/other/project");
    let (_broker, address) = start(config(&url)).await;

    let (status, body) = discover(address, "oc", Some(&cwd())).await;
    assert_eq!(status, 200);
    assert_eq!(body["server"]["version"], "1.19.3");
    assert_eq!(body["server"]["api"], "v1");
    let sessions = body["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1, "only the requested directory is listed");
    assert_eq!(sessions[0]["nativeId"], "ses_a");
    assert_eq!(sessions[0]["title"], "Fix the bug");
    assert!(sessions[0]["adoptedAs"].is_null());

    let id = adopt(address, "ses_a").await;
    let (_, body) = discover(address, "oc", Some(&cwd())).await;
    assert_eq!(body["sessions"][0]["adoptedAs"], id.to_string());
}

#[tokio::test]
async fn discovery_rejects_unknown_and_non_adoptable_profiles() {
    let (_fake, url) = fake_opencode::start(Options::default()).await;
    let mut config = config(&url);
    config.agents.insert(
        "plain".into(),
        Profile {
            kind: ProfileKind::Acp,
            command: Some("/bin/true".into()),
            endpoint: None,
            ..opencode_profile(&url, None)
        },
    );
    let (_broker, address) = start(config).await;
    assert_eq!(discover(address, "missing", None).await.0, 404);
    let (status, body) = discover(address, "plain", None).await;
    assert_eq!(status, 400);
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("opencode-server")
    );
}

#[tokio::test]
async fn adopting_a_native_session_never_creates_a_new_one_and_replays_its_history() {
    let (fake, url) = fake_opencode::start(Options::default()).await;
    fake.add_session("ses_a", "Fix the bug", &cwd());
    fake.add_exchange("ses_a", "why does it crash?", "a null pointer");
    let (_broker, address) = start(config(&url)).await;

    let id = adopt(address, "ses_a").await;

    assert_eq!(
        fake.exact("POST /session"),
        0,
        "adoption must not create a session"
    );
    assert_eq!(fake.exact("GET /session/ses_a"), 1);
    let summary = catalog(address).await["sessions"][0].clone();
    assert_eq!(summary["sessionId"], id.to_string());
    assert_eq!(summary["origin"], "adopted");
    assert_eq!(summary["nativeId"], "ses_a");
    assert_eq!(summary["title"], "Fix the bug");
    assert_eq!(summary["agent"], "oc");

    let history = replay(address, &id).await;
    assert_eq!(user_texts(&history), ["why does it crash?"]);
    assert_eq!(agent_text(&history), "a null pointer");
}

#[tokio::test]
async fn adopting_the_same_native_session_twice_points_at_the_existing_hive_session() {
    let (fake, url) = fake_opencode::start(Options::default()).await;
    fake.add_session("ses_a", "Fix the bug", &cwd());
    let (_broker, address) = start(config(&url)).await;
    let first = adopt(address, "ses_a").await;

    let error = connect(address, Pilot::default(), async |cx, _| {
        Ok(cx.send_request(adopt_request("ses_a")).block_task().await)
    })
    .await
    .expect_err("a second adoption must be refused");

    assert_eq!(data_code(&error), "hive::opencode::already_adopted");
    assert_eq!(
        error.data.as_ref().unwrap()["hiveSessionId"],
        first.to_string()
    );
    assert_eq!(
        catalog(address).await["sessions"].as_array().unwrap().len(),
        1
    );
    assert_eq!(fake.exact("POST /session"), 0);
}

#[tokio::test]
async fn adopting_a_session_that_does_not_exist_fails_instead_of_starting_a_new_one() {
    let (fake, url) = fake_opencode::start(Options::default()).await;
    let (_broker, address) = start(config(&url)).await;

    let error = connect(address, Pilot::default(), async |cx, _| {
        Ok(cx
            .send_request(adopt_request("ses_ghost"))
            .block_task()
            .await)
    })
    .await
    .expect_err("adoption of a missing session must fail");

    let text = error.data.as_ref().unwrap().to_string();
    assert!(text.contains("hive::opencode::session_not_found"), "{text}");
    assert_eq!(
        fake.exact("POST /session"),
        0,
        "never silently falls back to a new session"
    );
    assert!(
        catalog(address).await["sessions"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn adopting_with_the_wrong_directory_is_refused() {
    let (fake, url) = fake_opencode::start(Options::default()).await;
    fake.add_session("ses_a", "Elsewhere", "/some/other/project");
    let (_broker, address) = start(config(&url)).await;

    let error = connect(address, Pilot::default(), async |cx, _| {
        Ok(cx.send_request(adopt_request("ses_a")).block_task().await)
    })
    .await
    .expect_err("a session from another project must not be adopted");
    // The fake serves it for any directory, so the adapter's own check must catch it.
    assert!(
        error
            .data
            .as_ref()
            .unwrap()
            .to_string()
            .contains("directory_mismatch"),
        "{error:?}"
    );
}

#[tokio::test]
async fn only_opencode_server_profiles_can_adopt() {
    let mut config = config("http://127.0.0.1:1");
    config.agents.get_mut("oc").unwrap().kind = ProfileKind::Acp;
    config.agents.get_mut("oc").unwrap().endpoint = Some("http://127.0.0.1:1".into());
    let (_broker, address) = start(config).await;

    let error = connect(address, Pilot::default(), async |cx, _| {
        Ok(cx.send_request(adopt_request("ses_a")).block_task().await)
    })
    .await
    .expect_err("an ACP profile cannot adopt");
    assert!(
        error
            .data
            .as_ref()
            .unwrap()
            .to_string()
            .contains("opencode-server")
    );
}

// ---- prompting, control and cancellation ---------------------------------------------------

#[tokio::test]
async fn a_hive_created_session_is_a_real_native_session_whose_reply_streams_back() {
    let (fake, url) = fake_opencode::start(Options::default()).await;
    fake.script(Script::Reply("hello world".into()));
    let (_broker, address) = start(config(&url)).await;

    let seen = connect(address, Pilot::default(), async |cx, mut updates| {
        let opened = cx.send_request(new_request()).block_task().await?;
        let id = opened.session_id.clone();
        cx.send_request(prompt(&id, &opened.meta, "k1", "say hello"))
            .block_task()
            .await?;
        let seen = until(&mut updates, &id, is_idle).await;
        Ok(seen)
    })
    .await;

    assert_eq!(fake.exact("POST /session"), 1);
    assert_eq!(fake.prompts(), ["say hello"]);
    assert_eq!(agent_text(&seen), "hello world");
    assert_eq!(
        user_texts(&seen),
        ["say hello"],
        "the user message appears once"
    );
    let summary = catalog(address).await["sessions"][0].clone();
    assert_eq!(summary["origin"], "hive");
    assert_eq!(
        summary["nativeId"], "ses_new1",
        "resumable in OpenCode by this id"
    );
    assert_eq!(summary["activity"], "idle");
}

#[tokio::test]
async fn cancel_aborts_the_native_turn() {
    let (fake, url) = fake_opencode::start(Options::default()).await;
    fake.script(Script::Hold);
    let (_broker, address) = start(config(&url)).await;

    connect(address, Pilot::default(), async |cx, mut updates| {
        let opened = cx.send_request(new_request()).block_task().await?;
        let id = opened.session_id.clone();
        cx.send_request(prompt(&id, &opened.meta, "k1", "long task"))
            .block_task()
            .await?;
        until(&mut updates, &id, is_running).await;
        cx.send_notification(
            v2::CancelSessionNotification::new(id.clone()).meta(opened.meta.clone().unwrap()),
        )?;
        until(&mut updates, &id, is_idle).await;
        Ok(())
    })
    .await;

    assert_eq!(fake.exact("POST /session/ses_new1/abort"), 1);
}

#[tokio::test]
async fn a_turn_started_in_opencode_blocks_hive_prompts_until_it_ends() {
    let (fake, url) = fake_opencode::start(Options::default()).await;
    fake.add_session("ses_a", "Shared", &cwd());
    let (_broker, address) = start(config(&url)).await;

    connect(address, Pilot::default(), async |cx, _| {
        let opened = cx.send_request(adopt_request("ses_a")).block_task().await?;
        let id = opened.session_id.clone();

        fake.external_turn_start("ses_a");
        let snapshot = wait_for(address, |s| s["sessions"][0]["externalActivity"] == true).await;
        assert_eq!(snapshot["sessions"][0]["activity"], "running");

        let error = cx
            .send_request(prompt(&id, &opened.meta, "blocked", "hi"))
            .block_task()
            .await
            .expect_err("Hive must not add a second writer to a native turn");
        assert_eq!(data_code(&error), "hive::opencode::externally_controlled");
        assert!(fake.prompts().is_empty(), "nothing reached OpenCode");

        fake.external_turn_end("ses_a");
        wait_for(address, |s| s["sessions"][0]["externalActivity"] == false).await;
        cx.send_request(prompt(&id, &opened.meta, "allowed", "hi"))
            .block_task()
            .await?;
        assert_eq!(fake.prompts(), ["hi"]);
        Ok(())
    })
    .await;
}

// ---- permissions and questions -------------------------------------------------------------

#[tokio::test]
async fn a_permission_request_is_relayed_to_the_pilot_and_answered_in_opencode() {
    let (fake, url) = fake_opencode::start(Options::default()).await;
    let (_broker, address) = start(config(&url)).await;
    let pilot = Pilot {
        permission: Some("once"),
        ..Pilot::default()
    };

    connect(address, pilot.clone(), async |cx, _| {
        cx.send_request(new_request()).block_task().await?;
        fake.ask_permission("ses_new1", "per_1");
        timeout(Duration::from_secs(15), async {
            while fake.permission_replies().is_empty() {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("OpenCode never received the reply");
        Ok(())
    })
    .await;

    assert_eq!(
        fake.permission_replies(),
        [("per_1".to_owned(), json!({"reply": "once"}))]
    );
    let seen = pilot.seen.lock().unwrap();
    assert!(seen[0]["title"].as_str().unwrap().contains("bash"));
    let options: Vec<_> = seen[0]["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["optionId"].as_str().unwrap())
        .collect();
    assert_eq!(options, ["once", "always", "reject"]);
}

#[tokio::test]
async fn a_permission_answered_natively_first_is_not_answered_again_by_hive() {
    let (fake, url) = fake_opencode::start(Options::default()).await;
    let (_broker, address) = start(config(&url)).await;
    let pilot = Pilot::default(); // holds requests open

    connect(address, pilot.clone(), async |cx, _| {
        cx.send_request(new_request()).block_task().await?;
        fake.ask_permission("ses_new1", "per_1");
        timeout(Duration::from_secs(15), async {
            while pilot.seen.lock().unwrap().is_empty() {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("the pilot never saw the request");

        // The TUI user answers first; then the (slow) Hive pilot also answers.
        fake.answer_permission_natively("ses_new1", "per_1");
        tokio::time::sleep(Duration::from_millis(300)).await;
        pilot.release("once");
        tokio::time::sleep(Duration::from_millis(500)).await;
        Ok(())
    })
    .await;

    assert!(
        fake.permission_replies().is_empty(),
        "Hive must not reply to a request OpenCode already settled"
    );
}

#[tokio::test]
async fn a_question_is_relayed_as_a_form_and_the_answer_returns_to_opencode() {
    let (fake, url) = fake_opencode::start(Options::default()).await;
    let (_broker, address) = start(config(&url)).await;
    let pilot = Pilot {
        form: Some(BTreeMap::from([(
            "q0".to_owned(),
            v2::ElicitationContentValue::String("safe".into()),
        )])),
        ..Pilot::default()
    };

    connect(address, pilot.clone(), async |cx, _| {
        cx.send_request(new_request()).block_task().await?;
        fake.ask_question(
            "ses_new1",
            "que_1",
            json!([{"header": "Mode", "question": "Pick one",
                "options": [{"label": "fast"}, {"label": "safe"}]}]),
        );
        timeout(Duration::from_secs(15), async {
            while fake.question_replies().is_empty() {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("OpenCode never received the answer");
        Ok(())
    })
    .await;

    assert_eq!(
        fake.question_replies(),
        [(
            "que_1".to_owned(),
            "reply".to_owned(),
            json!({"answers": [["safe"]]})
        )]
    );
    let seen = pilot.seen.lock().unwrap();
    assert_eq!(
        seen[0]["requestedSchema"]["properties"]["q0"]["enum"][1],
        "safe"
    );
}

// ---- connection loss, restart and lifecycle -------------------------------------------------

#[tokio::test]
async fn a_dropped_event_stream_reconnects_and_recovers_the_text_it_missed() {
    let (fake, url) = fake_opencode::start(Options::default()).await;
    fake.add_session("ses_a", "Shared", &cwd());
    fake.add_exchange("ses_a", "greet me", "Hello");
    let (_broker, address) = start(config(&url)).await;
    let id = adopt(address, "ses_a").await;
    assert_eq!(agent_text(&replay(address, &id).await), "Hello");

    fake.kill_streams();
    // OpenCode kept writing while Hive was disconnected, and sent no event for it.
    fake.set_text("prt_a1", "Hello world");

    timeout(Duration::from_secs(20), async {
        loop {
            if agent_text(&replay(address, &id).await) == "Hello world" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("reconciliation never recovered the missed text");
    assert!(fake.exact("GET /event") >= 2, "the stream must be reopened");
    assert_eq!(fake.exact("POST /session"), 0);
}

fn temp_database() -> std::path::PathBuf {
    std::env::temp_dir()
        .join(format!("hive-opencode-{}", uuid::Uuid::new_v4()))
        .join("hive/sessions.sqlite3")
}

#[tokio::test]
async fn a_restart_reattaches_an_adopted_session_under_the_same_hive_id() {
    let (fake, url) = fake_opencode::start(Options::default()).await;
    fake.add_session("ses_a", "Shared", &cwd());
    fake.add_exchange("ses_a", "q", "a");
    let database = temp_database();
    let config = config(&url);

    let broker = hive_daemon::Broker::persistent(config.clone(), database.clone()).unwrap();
    let address = serve(broker.clone()).await;
    let id = adopt(address, "ses_a").await;
    broker.shutdown().await;
    drop(broker);

    let recovered = hive_daemon::Broker::persistent(config, database.clone()).unwrap();
    recovered.reconcile().await;
    let address = serve(recovered.clone()).await;
    let summary = catalog(address).await["sessions"][0].clone();
    assert_eq!(
        summary["sessionId"],
        id.to_string(),
        "identity survives the restart"
    );
    assert_eq!(summary["recovery"], "reattached");
    assert_eq!(summary["nativeId"], "ses_a");
    assert_eq!(summary["origin"], "adopted");
    assert_eq!(
        fake.exact("POST /session"),
        0,
        "recovery must not create a session"
    );
    recovered.shutdown().await;
    drop(recovered);

    let record: Value = serde_json::from_str(
        &rusqlite::Connection::open(&database)
            .unwrap()
            .query_row(
                "SELECT record FROM sessions WHERE id=?1",
                [id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
    )
    .unwrap();
    assert_eq!(record["kind"], "opencode-server");
    assert_eq!(
        record["api"], "v1",
        "the API flavor is pinned at first attach"
    );
    assert_eq!(record["origin"], "adopted");
    assert_eq!(record["downstream_id"], "ses_a");
}

#[tokio::test]
async fn a_session_deleted_while_hive_was_down_is_reported_lost_without_a_replacement() {
    let (fake, url) = fake_opencode::start(Options::default()).await;
    fake.add_session("ses_a", "Shared", &cwd());
    let database = temp_database();
    let config = config(&url);
    let broker = hive_daemon::Broker::persistent(config.clone(), database.clone()).unwrap();
    let address = serve(broker.clone()).await;
    adopt(address, "ses_a").await;
    broker.shutdown().await;
    drop(broker);

    fake.remove_session("ses_a");
    let recovered = hive_daemon::Broker::persistent(config, database).unwrap();
    recovered.reconcile().await;
    let address = serve(recovered).await;
    let summary = catalog(address).await["sessions"][0].clone();
    assert_eq!(summary["recovery"], "lost");
    assert!(
        summary["recoveryReason"]
            .as_str()
            .unwrap()
            .contains("hive::opencode::session_not_found")
    );
    assert_eq!(fake.exact("POST /session"), 0);
}

#[tokio::test]
async fn closing_detaches_hive_without_deleting_the_native_session() {
    let (fake, url) = fake_opencode::start(Options::default()).await;
    fake.add_session("ses_a", "Shared", &cwd());
    let (_broker, address) = start(config(&url)).await;

    connect(address, Pilot::default(), async |cx, _| {
        let opened = cx.send_request(adopt_request("ses_a")).block_task().await?;
        cx.send_request(v2::CloseSessionRequest::new(opened.session_id).meta(opened.meta.unwrap()))
            .block_task()
            .await?;
        Ok(())
    })
    .await;

    assert!(
        catalog(address).await["sessions"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        fake.count("DELETE"),
        0,
        "the OpenCode conversation must survive"
    );
    let (_, body) = discover(address, "oc", Some(&cwd())).await;
    assert_eq!(body["sessions"][0]["nativeId"], "ses_a");
    assert!(body["sessions"][0]["adoptedAs"].is_null());
    adopt(address, "ses_a").await; // and it can be adopted again
}

// ---- versions, credentials and API selection -----------------------------------------------

#[tokio::test]
async fn an_unsupported_opencode_version_fails_with_an_actionable_diagnostic() {
    let (fake, url) = fake_opencode::start(Options {
        version: "1.2.0",
        ..Options::default()
    })
    .await;
    fake.add_session("ses_a", "Old", &cwd());
    let (_broker, address) = start(config(&url)).await;

    let (status, body) = discover(address, "oc", Some(&cwd())).await;
    assert_eq!(status, 502);
    assert_eq!(body["code"], "hive::opencode::unsupported_version");
    assert!(body["message"].as_str().unwrap().contains("1.2.0"));
    assert!(body["help"].as_str().unwrap().contains("upgrade"));

    let error = connect(address, Pilot::default(), async |cx, _| {
        Ok(cx.send_request(adopt_request("ses_a")).block_task().await)
    })
    .await
    .expect_err("an unsupported server must not be used");
    assert_eq!(data_code(&error), "hive::opencode::unsupported_version");
    assert_eq!(fake.exact("POST /session"), 0);
}

#[tokio::test]
async fn an_unreachable_server_is_reported_as_such() {
    let (_broker, address) = start(config("http://127.0.0.1:1")).await;
    let (status, body) = discover(address, "oc", None).await;
    assert_eq!(status, 502);
    assert_eq!(body["code"], "hive::opencode::unreachable");
    assert!(
        !body.to_string().contains("127.0.0.1"),
        "the endpoint must not leak into diagnostics"
    );
}

#[tokio::test]
async fn credentials_are_sent_when_configured_and_never_echoed_back() {
    // PATH always exists, which avoids mutating the process environment from a test.
    let password = std::env::var("PATH").unwrap();
    let (fake, url) = fake_opencode::start(Options {
        password: Some(password.clone()),
        ..Options::default()
    })
    .await;
    fake.add_session("ses_a", "Locked", &cwd());

    let (_broker, address) = start(config_with(opencode_profile(&url, Some("PATH")))).await;
    let (status, body) = discover(address, "oc", Some(&cwd())).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["sessions"][0]["nativeId"], "ses_a");
    let providers = reqwest::get(format!("http://{address}/v1/providers"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        !providers.contains(&url) && !providers.contains("PATH"),
        "{providers}"
    );

    let (_broker, address) = start(config(&url)).await; // no password_env
    let (status, body) = discover(address, "oc", None).await;
    assert_eq!(status, 502);
    assert_eq!(body["code"], "hive::opencode::unauthorized");
    let text = body.to_string();
    assert!(!text.contains(&password) && !text.contains(&url));
}

#[tokio::test]
async fn a_password_variable_that_is_not_set_is_named_in_the_diagnostic() {
    let (_fake, url) = fake_opencode::start(Options::default()).await;
    let profile = opencode_profile(&url, Some("HIVE_TEST_SURELY_UNSET_VARIABLE"));
    let (_broker, address) = start(config_with(profile)).await;
    let (status, body) = discover(address, "oc", None).await;
    assert_eq!(status, 502);
    assert_eq!(body["code"], "hive::opencode::missing_credentials");
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("HIVE_TEST_SURELY_UNSET_VARIABLE")
    );
}

#[tokio::test]
async fn an_html_catch_all_is_not_mistaken_for_a_supported_api() {
    let (_fake, url) = fake_opencode::start(Options {
        v1: false,
        v2: false,
        html_fallback: true,
        ..Options::default()
    })
    .await;
    let (_broker, address) = start(config(&url)).await;
    let (status, body) = discover(address, "oc", None).await;
    assert_eq!(status, 502);
    assert_eq!(body["code"], "hive::opencode::api_unavailable");
}

#[tokio::test]
async fn a_v2_only_server_is_driven_through_the_preview_api_with_a_durable_cursor() {
    let (fake, url) = fake_opencode::start(Options {
        v1: false,
        v2: true,
        ..Options::default()
    })
    .await;
    fake.add_session("ses_v2", "Preview", &cwd());
    fake.add_v2_exchange("ses_v2", "earlier question", "earlier answer");
    fake.script(Script::Reply("fresh reply".into()));
    let (_broker, address) = start(config(&url)).await;

    let (history, seen) = connect(address, Pilot::default(), async |cx, mut updates| {
        let opened = cx
            .send_request(adopt_request("ses_v2"))
            .block_task()
            .await?;
        let id = opened.session_id.clone();
        let history = replay(address, &id).await;
        cx.send_request(prompt(&id, &opened.meta, "k1", "go on"))
            .block_task()
            .await?;
        let seen = until(&mut updates, &id, is_idle).await;
        Ok((history, seen))
    })
    .await;

    assert_eq!(user_texts(&history), ["earlier question"]);
    assert_eq!(agent_text(&history), "earlier answer");
    assert_eq!(agent_text(&seen), "fresh reply");
    assert_eq!(fake.exact("POST /session"), 0);
    assert_eq!(fake.exact("POST /api/session/ses_v2/prompt"), 1);
    assert!(
        fake.calls()
            .contains(&"GET /api/session/ses_v2/event after=2".to_owned()),
        "the stream must resume from the history's last durable sequence: {:?}",
        fake.calls()
    );
}

#[tokio::test]
async fn a_new_session_on_a_v2_only_server_is_created_through_the_preview_api() {
    let (fake, url) = fake_opencode::start(Options {
        v1: false,
        v2: true,
        ..Options::default()
    })
    .await;
    let (_broker, address) = start(config(&url)).await;
    connect(address, Pilot::default(), async |cx, _| {
        cx.send_request(new_request()).block_task().await?;
        Ok(())
    })
    .await;
    assert_eq!(fake.exact("POST /api/session"), 1);
    assert_eq!(fake.exact("POST /session"), 0);
    let summary = catalog(address).await["sessions"][0].clone();
    assert_eq!(summary["origin"], "hive");
    assert!(
        summary["nativeId"]
            .as_str()
            .unwrap()
            .starts_with("ses_v2new")
    );
}
