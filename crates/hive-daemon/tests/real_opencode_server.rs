//! Opt-in probe of a real OpenCode server: never launches a model during routine CI.
//!
//! ```sh
//! HIVE_REAL_OPENCODE_SERVER=1 cargo test -p hive-daemon --test real_opencode_server -- --ignored --nocapture
//! ```
//!
//! By default it starts its own throwaway `opencode serve` with temporary XDG directories and
//! the free `opencode/big-pickle` model, so it touches neither your sessions nor your
//! credentials. Point it at an existing server with `HIVE_REAL_OPENCODE_SERVER_URL` (and
//! `HIVE_REAL_OPENCODE_SERVER_PASSWORD_ENV` naming the variable that holds its password).
//!
//! The `PROBE`, `NATIVE`, `ADOPTED`, `FIRST` and `RESUMED` markers on stderr are the evidence
//! that fills the OpenCode server capability matrix in docs/acp-compatibility.md.

use std::{
    collections::BTreeMap,
    net::SocketAddr,
    path::PathBuf,
    process::Stdio,
    time::{Duration, Instant},
};

use agent_client_protocol::{
    Agent, Client, Error, V2ConnectionTo,
    schema::{ProtocolVersion, v2},
};
use agent_client_protocol_http::HttpClient;
use hive_daemon::config::{Config, Profile, ProfileKind};
use hive_protocol::AgentVersion;
use serde_json::{Value, json};
use tokio::{sync::mpsc, time::timeout};

const TURN_TIMEOUT: Duration = Duration::from_secs(120);

struct Server {
    url: String,
    password_env: Option<String>,
    // Held so the throwaway server dies with the test, even on panic.
    _child: Option<tokio::process::Child>,
    _state: Option<Scratch>,
}

/// Removes the throwaway server's XDG tree, which holds its session database.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Fixed labels only: server error text can contain paths, prompts or tokens.
fn fail(stage: &str) -> ! {
    panic!("real OpenCode server probe failed at: {stage}");
}

async fn server() -> Option<Server> {
    if std::env::var("HIVE_REAL_OPENCODE_SERVER").as_deref() != Ok("1") {
        eprintln!(
            "SKIP opencode-server: set HIVE_REAL_OPENCODE_SERVER=1 and run this ignored test explicitly"
        );
        return None;
    }
    if let Ok(url) = std::env::var("HIVE_REAL_OPENCODE_SERVER_URL") {
        return Some(Server {
            url,
            password_env: std::env::var("HIVE_REAL_OPENCODE_SERVER_PASSWORD_ENV").ok(),
            _child: None,
            _state: None,
        });
    }
    let command =
        std::env::var("HIVE_REAL_OPENCODE_SERVER_COMMAND").unwrap_or_else(|_| "opencode".into());
    let state = std::env::temp_dir().join(format!("hive-real-opencode-{}", uuid::Uuid::new_v4()));
    for directory in ["home", "data", "config/opencode", "state", "cache"] {
        std::fs::create_dir_all(state.join(directory)).unwrap();
    }
    // Without a model the server's default needs credentials the throwaway profile lacks.
    std::fs::write(
        state.join("config/opencode/opencode.json"),
        json!({"model": "opencode/big-pickle"}).to_string(),
    )
    .unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let child = tokio::process::Command::new(&command)
        .args([
            "serve",
            "--hostname",
            "127.0.0.1",
            "--port",
            &port.to_string(),
        ])
        .env("HOME", state.join("home"))
        .env("XDG_DATA_HOME", state.join("data"))
        .env("XDG_CONFIG_HOME", state.join("config"))
        .env("XDG_STATE_HOME", state.join("state"))
        .env("XDG_CACHE_HOME", state.join("cache"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn();
    let Ok(child) = child else {
        eprintln!(
            "SKIP opencode-server: executable unavailable; set HIVE_REAL_OPENCODE_SERVER_COMMAND"
        );
        return None;
    };
    let url = format!("http://127.0.0.1:{port}");
    let started = Instant::now();
    loop {
        // The port can accept connections before the server loop answers them; a short timeout
        // turns such a stalled attempt into a retry on a fresh connection.
        let probe = reqwest::Client::new()
            .get(format!("{url}/global/health"))
            .timeout(Duration::from_secs(2))
            .send()
            .await;
        if probe.is_ok_and(|response| response.status().is_success()) {
            break;
        }
        if started.elapsed() > Duration::from_secs(30) {
            fail("the throwaway opencode serve never became healthy");
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    Some(Server {
        url,
        password_env: None,
        _child: Some(child),
        _state: Some(Scratch(state)),
    })
}

fn credentials(server: &Server, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    match server
        .password_env
        .as_ref()
        .and_then(|name| std::env::var(name).ok())
    {
        Some(password) => request.basic_auth("opencode", Some(password)),
        None => request,
    }
}

async fn native_json(
    server: &Server,
    method: reqwest::Method,
    path: &str,
    body: Option<Value>,
) -> Value {
    // A wedged server must fail the probe, not hang it.
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();
    let mut request = credentials(
        server,
        client.request(method, format!("{}{path}", server.url)),
    );
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request
        .send()
        .await
        .unwrap_or_else(|_| fail("native request"));
    if !response.status().is_success() {
        fail("native request status");
    }
    response.json().await.unwrap_or(Value::Null)
}

async fn wait_idle(server: &Server, directory: &str) {
    let path = format!("/session/status?directory={}", encode(directory));
    let started = Instant::now();
    // A turn is visible as busy shortly after admission; give it time to appear first.
    tokio::time::sleep(Duration::from_secs(2)).await;
    loop {
        let status = native_json(server, reqwest::Method::GET, &path, None).await;
        if status.as_object().is_none_or(|busy| busy.is_empty()) {
            return;
        }
        if started.elapsed() > TURN_TIMEOUT {
            fail("native turn never finished; check the model and provider");
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

fn encode(text: &str) -> String {
    reqwest::Url::parse_with_params("http://x/", [("q", text)])
        .unwrap()
        .query()
        .unwrap()
        .trim_start_matches("q=")
        .to_owned()
}

fn config(server: &Server) -> Config {
    Config {
        default_agent: Some("oc".into()),
        agents: BTreeMap::from([(
            "oc".into(),
            Profile {
                name: None,
                kind: ProfileKind::OpencodeServer,
                command: None,
                args: vec![],
                env: BTreeMap::new(),
                endpoint: Some(server.url.clone()),
                headers: BTreeMap::new(),
                versions: vec![AgentVersion::V2],
                username: None,
                password_env: server.password_env.clone(),
                pricing: Default::default(),
            },
        )]),
    }
}

async fn text_turn(
    cx: &V2ConnectionTo<Agent>,
    updates: &mut mpsc::UnboundedReceiver<v2::UpdateSessionNotification>,
    id: &v2::SessionId,
    meta: &Option<v2::Meta>,
    key: &str,
    prompt: &str,
) -> String {
    let mut owner = meta.clone().expect("ownership metadata");
    owner.get_mut("hive").expect("hive metadata")["key"] = key.into();
    cx.send_request(v2::PromptRequest::new(id.clone(), vec![prompt.into()]).meta(owner))
        .block_task()
        .await
        .unwrap_or_else(|_| fail("Hive session/prompt"));
    let mut text = String::new();
    timeout(TURN_TIMEOUT, async {
        while let Some(update) = updates.recv().await {
            if &update.session_id != id {
                continue;
            }
            match update.update {
                v2::SessionUpdate::AgentMessageChunk(chunk) => {
                    if let v2::ContentBlock::Text(content) = chunk.content {
                        text.push_str(&content.text);
                    }
                }
                v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(_)) => return,
                _ => {}
            }
        }
        fail("the Hive connection ended before idle");
    })
    .await
    .unwrap_or_else(|_| fail("no idle update before the turn timeout"));
    text
}

#[tokio::test]
#[ignore = "requires an OpenCode server and a model; set HIVE_REAL_OPENCODE_SERVER=1"]
async fn a_session_started_natively_in_opencode_is_continued_through_hive_without_a_duplicate() {
    let Some(server) = server().await else {
        return;
    };
    let directory = std::env::current_dir().unwrap().display().to_string();
    eprintln!("STAGE server ready");

    // Act as the OpenCode TUI: create a session and run a turn natively.
    let created = native_json(
        &server,
        reqwest::Method::POST,
        &format!("/session?directory={}", encode(&directory)),
        Some(json!({})),
    )
    .await;
    let native_id = created["id"]
        .as_str()
        .unwrap_or_else(|| fail("native session id"))
        .to_owned();
    native_json(
        &server,
        reqwest::Method::POST,
        &format!("/session/{native_id}/prompt_async?directory={}", encode(&directory)),
        Some(json!({"parts": [{"type": "text", "text": "Remember the word LANTERN. Reply with one short sentence."}]})),
    )
    .await;
    eprintln!("STAGE native prompt admitted");
    wait_idle(&server, &directory).await;
    eprintln!("STAGE native turn finished");
    let sessions_before = native_json(
        &server,
        reqwest::Method::GET,
        &format!("/session?directory={}", encode(&directory)),
        None,
    )
    .await
    .as_array()
    .map_or(0, Vec::len);

    let broker = hive_daemon::Broker::new(config(&server));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(axum::serve(listener, hive_daemon::router(broker.clone())).into_future());

    let discovery: Value = reqwest::get(format!(
        "http://{address}/v1/providers/oc/native-sessions?directory={}",
        encode(&directory)
    ))
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    if discovery["server"]["version"].is_null() {
        fail("Hive discovery against the real server");
    }
    eprintln!(
        "PROBE server=opencode version={} api={} adapter=rest+sse upstream=ws,http+sse",
        discovery["server"]["version"].as_str().unwrap_or("unknown"),
        discovery["server"]["api"].as_str().unwrap_or("unknown"),
    );
    if !discovery["sessions"]
        .as_array()
        .is_some_and(|sessions| sessions.iter().any(|s| s["nativeId"] == native_id.as_str()))
    {
        fail("the natively created session was not discovered");
    }
    eprintln!("NATIVE discovered=true native_id_format=ses_*");

    let adopted_native = native_id.clone();
    let (tx, mut updates) = mpsc::unbounded_channel();
    let result = timeout(
        Duration::from_secs(300),
        Client
            .v2()
            .on_receive_notification(
                async move |update: v2::UpdateSessionNotification, _: V2ConnectionTo<Agent>| {
                    tx.send(update).map_err(Error::into_internal_error)
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(
                HttpClient::new(format!("ws://{address}")).unwrap(),
                async move |cx| {
                    cx.send_request(v2::InitializeRequest::new(
                        ProtocolVersion::V2,
                        v2::Implementation::new("real-opencode-server", "1"),
                    ))
                    .block_task()
                    .await?;
                    let mut meta = v2::Meta::new();
                    meta.insert(
                        "hive".into(),
                        json!({"agent": "oc", "adopt": {"nativeId": adopted_native}}),
                    );
                    let opened = cx
                        .send_request(
                            v2::NewSessionRequest::new(std::env::current_dir().unwrap()).meta(meta),
                        )
                        .block_task()
                        .await?;
                    eprintln!("ADOPTED hive_session=created native_session_reused=true");
                    let id = opened.session_id.clone();
                    let reply = text_turn(
                        &cx,
                        &mut updates,
                        &id,
                        &opened.meta,
                        "recall",
                        "What word did I ask you to remember? Answer with just the word.",
                    )
                    .await;
                    eprintln!(
                        "FIRST text_observed={} recalled_context={}",
                        !reply.trim().is_empty(),
                        reply.to_uppercase().contains("LANTERN")
                    );
                    Ok((reply, opened))
                },
            ),
    )
    .await;
    let Ok(Ok((reply, _))) = result else {
        fail("adoption and first Hive turn");
    };
    if reply.trim().is_empty() {
        fail("the adopted session produced no assistant text");
    }

    // Native view: still one session, and it now contains the turn Hive ran.
    let sessions_after = native_json(
        &server,
        reqwest::Method::GET,
        &format!("/session?directory={}", encode(&directory)),
        None,
    )
    .await
    .as_array()
    .map_or(0, Vec::len);
    let messages = native_json(
        &server,
        reqwest::Method::GET,
        &format!(
            "/session/{native_id}/message?directory={}",
            encode(&directory)
        ),
        None,
    )
    .await
    .as_array()
    .map_or(0, Vec::len);
    eprintln!(
        "RESUMED duplicate_session={} native_messages={messages} resumable_in_opencode=true",
        sessions_after != sessions_before
    );
    assert_eq!(
        sessions_after, sessions_before,
        "Hive created a second session"
    );
    assert!(messages >= 4, "the native history must contain both turns");
    let _ = timeout(Duration::from_secs(8), broker.shutdown()).await;
}
