//! Opt-in compatibility probes: never launch a paid agent during routine CI.

use std::{collections::BTreeMap, net::SocketAddr, path::PathBuf, process::Stdio, time::Duration};

use agent_client_protocol::{
    AcpAgent, AcpAgentConfig, Agent, Client, ConnectionTo, Error, V2ConnectionTo,
    schema::{MaybeUndefined, ProtocolVersion, v1, v2},
};
use agent_client_protocol_http::HttpClient;
use hive_daemon::config::{Config, Profile};
use hive_protocol::AgentVersion;
use tokio::{sync::mpsc, time::timeout};

const TURN_TIMEOUT: Duration = Duration::from_secs(120);
const PROBE_PROMPT: &str =
    "Reply with one short sentence about the weather on Mars. Do not use tools or read files.";

struct Installation {
    id: &'static str,
    command: String,
    args: Vec<String>,
    version: String,
    wire: AgentVersion,
}

/// The launch command: an explicit override, otherwise the documented entrypoint for `id`.
fn launch(id: &str, prefix: &str) -> (String, Vec<String>) {
    let command = std::env::var(format!("{prefix}_COMMAND")).ok();
    let args = std::env::var(format!("{prefix}_ARGS_JSON"))
        .ok()
        .map(|value| {
            serde_json::from_str::<Vec<String>>(&value)
                .expect("ARGS_JSON must be a JSON array of separate argument strings")
        });
    let (default_command, default_args): (&str, &[&str]) = match id {
        "opencode" => ("opencode", &["acp"]),
        "claude" => ("claude-agent-acp", &[]),
        "codex" => ("codex-acp", &[]),
        // Prefer an installed adapter; otherwise let npx fetch it, as the adapter's README does.
        _ if hive_daemon::compat::check_launch("pi-acp", &[]).is_ok() => ("pi-acp", &[]),
        _ => ("npx", &["-y", "pi-acp"]),
    };
    match (command, args) {
        (Some(command), Some(args)) => (command, args),
        // Default arguments only belong to the default command.
        (Some(command), None) if id == "opencode" => (
            command,
            default_args.iter().map(ToString::to_string).collect(),
        ),
        (Some(command), None) => (command, Vec::new()),
        (None, args) => (
            default_command.into(),
            args.unwrap_or_else(|| default_args.iter().map(ToString::to_string).collect()),
        ),
    }
}

async fn installation(id: &'static str) -> Option<Installation> {
    let prefix = format!("HIVE_REAL_{}", id.to_ascii_uppercase());
    if std::env::var(&prefix).as_deref() != Ok("1") {
        eprintln!("SKIP {id}: set {prefix}=1 and run this ignored test explicitly");
        return None;
    }
    let (command, args) = launch(id, &prefix);
    let wire = match std::env::var(format!("{prefix}_WIRE")) {
        Ok(value) if value == "v2" => AgentVersion::V2,
        Ok(value) if value == "v1" => AgentVersion::V1,
        // Both known adapters (OpenCode, pi-acp) negotiate ACPv1 today.
        Err(_) => AgentVersion::V1,
        _ => panic!("{prefix}_WIRE must be v1 or v2; verify the agent's actual ACP protocol"),
    };
    // pi-acp is a stdio adapter with no `--version`; the product version is Pi's own.
    let version_command = if id == "pi" {
        "pi".to_string()
    } else {
        command.clone()
    };
    let result = timeout(
        Duration::from_secs(5),
        tokio::process::Command::new(&version_command)
            .arg("--version")
            .stderr(Stdio::null())
            .output(),
    )
    .await;
    let output = match result {
        Ok(Ok(output)) if output.status.success() => output,
        Ok(Err(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            eprintln!(
                "SKIP {id}: `{version_command}` unavailable; install it or set {prefix}_COMMAND to an installed binary"
            );
            return None;
        }
        _ => {
            eprintln!(
                "SKIP {id}: product version unavailable; verify `{version_command} --version` works"
            );
            return None;
        }
    };
    let version = String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .unwrap_or("unknown")
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || ".-_+".contains(*ch))
        .take(64)
        .collect::<String>();
    if version.is_empty() {
        eprintln!("SKIP {id}: --version did not return a product version");
        return None;
    }
    eprintln!(
        "PROBE agent={id} version={version} downstream=stdio wire={wire:?} upstream=ws,http+sse"
    );
    Some(Installation {
        id,
        command,
        args,
        version,
        wire,
    })
}

fn owned(meta: &Option<v2::Meta>, key: &str) -> v2::Meta {
    let mut meta = meta.clone().expect("Hive must return ownership metadata");
    if !key.is_empty() {
        meta.get_mut("hive").expect("Hive ownership metadata")["key"] = key.into();
    }
    meta
}

fn diagnostic(stage: &str) -> Error {
    Error::internal_error().data(stage)
}

fn stage(error: Error) -> String {
    // Remote JSON-RPC error data can contain paths, tokens, or prompts; show only our own labels.
    match error.data.and_then(|data| data.as_str().map(str::to_owned)) {
        Some(label)
            if matches!(
                label.as_str(),
                "Hive initialize failed"
                    | "Hive did not advertise v2 session support"
                    | "real agent session/new failed; inspect wire version, ACP entrypoint and agent configuration"
                    | "first turn completed without observable assistant text"
                    | "second Hive initialize failed"
                    | "Hive session/list failed"
                    | "Hive did not list its disconnected live session"
                    | "Hive live-worker session/resume failed"
                    | "resumed turn completed without observable assistant text"
                    | "Hive session/close failed; check agent close support"
                    | "native v1 initialize failed"
                    | "native v2 initialize failed"
                    | "native agent did not negotiate ACPv1"
                    | "native agent did not negotiate ACPv2"
                    | "native agent does not advertise history restoration via session/load"
                    | "native agent does not advertise session support for history restoration"
                    | "session/load rejected native ID; check cwd and native ACP load capability"
                    | "session/resume rejected native ID; check cwd and native ACP resume capability"
                    | "session/prompt acknowledgement timed out"
                    | "session/prompt failed; check model credentials, protocol and agent logs"
                    | "no idle update before turn timeout; cancel or inspect agent logs"
                    | "agent connection ended before idle"
            ) =>
        {
            label
        }
        _ => "upstream connection closed; inspect transport and agent availability".into(),
    }
}

fn workspace(id: &str) -> Result<PathBuf, String> {
    let key = format!("HIVE_REAL_{}_CWD", id.to_ascii_uppercase());
    let cwd = match std::env::var(&key) {
        Ok(path) => PathBuf::from(path),
        Err(_) => std::env::current_dir().map_err(|_| "cannot resolve working directory")?,
    };
    if !cwd.is_absolute() || !cwd.is_dir() {
        return Err(format!(
            "{key} must identify an existing absolute directory"
        ));
    }
    Ok(cwd)
}

#[derive(Default)]
struct Observed {
    text: bool,
    answer: String,
    running: bool,
    chunks: bool,
    snapshots: bool,
    tools: bool,
}

async fn turn(
    cx: &V2ConnectionTo<Agent>,
    rx: &mut mpsc::UnboundedReceiver<v2::UpdateSessionNotification>,
    id: &v2::SessionId,
    owner: &Option<v2::Meta>,
    key: &str,
) -> Result<Observed, String> {
    let request =
        v2::PromptRequest::new(id.clone(), vec![PROBE_PROMPT.into()]).meta(owned(owner, key));
    timeout(TURN_TIMEOUT, cx.send_request(request).block_task())
        .await
        .map_err(|_| "session/prompt acknowledgement timed out".to_string())?
        .map_err(|_| {
            "session/prompt failed; check model credentials, protocol and agent logs".to_string()
        })?;
    let mut observed = Observed::default();
    timeout(TURN_TIMEOUT, async {
        while let Some(update) = rx.recv().await {
            if update.session_id != *id { continue; }
            match update.update {
                v2::SessionUpdate::AgentMessageChunk(chunk) => {
                    observed.chunks = true;
                    if let v2::ContentBlock::Text(text) = chunk.content {
                        observed.text |= !text.text.trim().is_empty();
                        observed.answer.push_str(&text.text);
                    }
                }
                v2::SessionUpdate::AgentMessage(message) => {
                    observed.snapshots = true;
                    if let MaybeUndefined::Value(content) = message.content {
                        observed.text |= content.iter().any(|content| {
                            matches!(content, v2::ContentBlock::Text(text) if !text.text.trim().is_empty())
                        });
                        observed.answer = content.iter().filter_map(|block| match block {
                            v2::ContentBlock::Text(text) => Some(text.text.as_str()),
                            _ => None,
                        }).collect();
                    }
                }
                v2::SessionUpdate::ToolCallUpdate(_) => observed.tools = true,
                v2::SessionUpdate::StateUpdate(v2::StateUpdate::Running(_)) => observed.running = true,
                // Startup and resume barriers report current idle without ending the prompt
                // just sent; only a terminal stop reason is evidence this turn completed.
                v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(idle)) if idle.stop_reason.is_some() => return Ok(observed),
                _ => {}
            }
        }
        Err("agent connection ended before idle".into())
    }).await.map_err(|_| "no idle update before turn timeout; cancel or inspect agent logs".to_string())?
}

async fn run(installation: Installation) -> Result<(), String> {
    run_probe(installation, false).await
}

async fn run_probe(installation: Installation, configuration: bool) -> Result<(), String> {
    let profile = Profile {
        name: None,
        command: Some(installation.command),
        args: installation.args,
        env: BTreeMap::new(),
        endpoint: None,
        headers: BTreeMap::new(),
        versions: vec![installation.wire],
        pricing: Default::default(),
        kind: Default::default(),
        username: None,
        password_env: None,
    };
    let mut agents = BTreeMap::new();
    agents.insert(installation.id.into(), profile);
    let directory = tempfile::tempdir().map_err(|_| "cannot create probe state directory")?;
    let database = directory.path().join("hive/sessions.sqlite3");
    let broker = hive_daemon::Broker::persistent(
        Config {
            default_agent: Some(installation.id.into()),
            agents,
        },
        database.clone(),
    )
    .map_err(|_| "cannot open probe session database")?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|_| "cannot bind loopback listener")?;
    let address: SocketAddr = listener
        .local_addr()
        .map_err(|_| "cannot inspect loopback listener")?;
    let origin = format!("http://{address}");
    let server = tokio::spawn(
        axum::serve(
            listener,
            hive_daemon::router_with_origin(broker.clone(), Some(&origin)),
        )
        .into_future(),
    );
    // Isolate probe panics so even malformed agent replies cannot bypass worker cleanup.
    let mut flow = tokio::spawn(async move {
        if configuration {
            configuration_lifecycle(address, installation.id).await
        } else {
            lifecycle(address, installation.id, database).await
        }
    });
    let result = timeout(Duration::from_secs(300), &mut flow).await;
    if result.is_err() {
        flow.abort();
    }
    // A disconnected controller leaves its worker alive; always drain it on errors and timeouts.
    let _ = timeout(Duration::from_secs(8), broker.shutdown()).await;
    server.abort();
    match result {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => {
            Err("real-agent probe panicked; inspect agent replies and gateway logs".into())
        }
        Err(_) => {
            Err("real-agent lifecycle timed out; check credentials or model availability".into())
        }
    }
}

async fn configuration_lifecycle(address: SocketAddr, id: &str) -> Result<(), String> {
    let cwd = workspace(id)?;
    Client.v2().connect_with(HttpClient::new(format!("ws://{address}")).map_err(|_| "invalid WS URL")?, async move |cx| {
        cx.send_request(v2::InitializeRequest::new(ProtocolVersion::V2, v2::Implementation::new("configuration-probe", "1"))).block_task().await?;
        let opened = cx.send_request(v2::NewSessionRequest::new(cwd.clone())).block_task().await?;
        let requested = std::env::var(format!("HIVE_REAL_{}_CONFIG_ID", id.to_ascii_uppercase())).ok();
        let option = opened.config_options.iter().find(|option| {
            let value = serde_json::to_value(option).unwrap_or_default();
            value["type"] == "select" && requested.as_ref().map_or_else(
                || value["category"] == "mode" || value["category"] == "thought_level" || value["category"] == "collaboration_mode",
                |config| option.config_id.to_string() == *config,
            )
        }).ok_or_else(|| diagnostic("no advertised session setting; inspect adapter capabilities"))?;
        let value = serde_json::to_value(option).map_err(Error::into_internal_error)?;
        let original = value["currentValue"].as_str().ok_or_else(Error::invalid_params)?;
        let choices: Vec<_> = value["options"].as_array().into_iter().flatten().flat_map(|choice| {
            choice["options"].as_array().map_or_else(|| vec![choice], |items| items.iter().collect())
        }).collect();
        let target = choices.iter().filter_map(|choice| choice["value"].as_str()).find(|choice| *choice != original)
            .ok_or_else(|| diagnostic("no alternate advertised setting value"))?;
        let setter = |value: &str| v2::SetSessionConfigOptionRequest::new(opened.session_id.clone(), option.config_id.clone(), v2::SessionConfigOptionValue::Id { value: v2::SessionConfigValueId::new(value.to_owned()) }).meta(opened.meta.clone());
        let changed = cx.send_request(setter(target)).block_task().await;
        // Attempt restoration even if the first acknowledgement failed: the remote side may have applied it.
        let restored = cx.send_request(setter(original)).block_task().await?;
        let changed = changed?;
        let current = |options: &[v2::SessionConfigOption]| options.iter().find(|item| item.config_id == option.config_id)
            .and_then(|item| serde_json::to_value(item).ok()).and_then(|item| item["currentValue"].as_str().map(str::to_owned));
        if current(&changed.config_options).as_deref() != Some(target) || current(&restored.config_options).as_deref() != Some(original) {
            return Err(diagnostic("agent did not confirm changed and restored settings"));
        }
        let attached = cx.send_request(v2::ResumeSessionRequest::new(opened.session_id, cwd)).block_task().await?;
        if current(&attached.config_options).as_deref() != Some(original) { return Err(diagnostic("reattachment did not retain restored setting")); }
        eprintln!("CONFIG agent={id} config={} changed=true restored=true reattached=true inference=false", option.config_id);
        Ok::<_, Error>(())
    }).await.map_err(|error| format!("configuration probe failed (code {:?}); inspect adapter configuration and logs", error.code))
}

#[tokio::test]
#[ignore = "requires explicit HIVE_REAL_<AGENT>=1 and an authenticated installed adapter"]
async fn real_agent_settings_can_change_and_restore_without_inference() {
    for id in ["opencode", "claude", "codex", "pi"] {
        if let Some(installation) = installation(id).await {
            run_probe(installation, true).await.unwrap();
        }
    }
}

fn saved_identity(database: &std::path::Path, session: &v2::SessionId) -> serde_json::Value {
    let db =
        rusqlite::Connection::open_with_flags(database, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let record: String = db
        .query_row(
            "SELECT record FROM sessions WHERE id=?1",
            [session.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    let record: serde_json::Value = serde_json::from_str(&record).unwrap();
    serde_json::json!({"id":record["id"], "nativeId":record["downstream_id"], "cwd":record["cwd"]})
}

async fn replay_snapshot(
    address: SocketAddr,
    scheme: &str,
    session: v2::SessionId,
    cwd: PathBuf,
) -> Result<Vec<serde_json::Value>, String> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let boundary = tx.clone();
    let permissions = tx.clone();
    timeout(
        Duration::from_secs(30),
        Client
            .v2()
            .on_receive_notification(
                async move |update: v2::UpdateSessionNotification, _: V2ConnectionTo<Agent>| {
                    tx.send(Some(
                        serde_json::to_value(update).map_err(Error::into_internal_error)?,
                    ))
                    .map_err(Error::into_internal_error)
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .on_receive_request(
                async move |_: v2::RequestPermissionRequest,
                            responder: agent_client_protocol::Responder<
                    v2::RequestPermissionResponse,
                >,
                            _: V2ConnectionTo<Agent>| {
                    permissions
                        .send(Some(serde_json::json!({"unexpectedPermission":true})))
                        .map_err(Error::into_internal_error)?;
                    responder.respond_with_error(Error::invalid_request())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(
                HttpClient::new(format!("{scheme}://{address}"))
                    .map_err(|_| "invalid replay URL")?,
                async move |cx| {
                    cx.send_request(v2::InitializeRequest::new(
                        ProtocolVersion::V2,
                        v2::Implementation::new("replay-probe", "1"),
                    ))
                    .block_task()
                    .await?;
                    let listed = cx
                        .send_request(v2::ListSessionsRequest::new())
                        .block_task()
                        .await?;
                    assert_eq!(
                        listed.sessions.len(),
                        1,
                        "reopening must not replace or fork the conversation"
                    );
                    assert!(
                        listed.sessions[0].session_id == session,
                        "reopening changed the session identity"
                    );
                    assert!(
                        serde_json::to_value(&listed.sessions[0]).unwrap()["cwd"]
                            == cwd.to_str().unwrap(),
                        "reopening changed the workspace"
                    );
                    let (done, response) = tokio::sync::oneshot::channel();
                    cx.prepare_request(
                        v2::ResumeSessionRequest::new(session.clone(), cwd)
                            .replay_from(v2::ReplayFrom::Start(v2::ReplayFromStart::new()))
                            .meta(v2::Meta::from_iter([(
                                "hive".into(),
                                serde_json::json!({"mode":"observe"}),
                            )])),
                    )
                    .on_receiving_result(async move |result| {
                        // Close the batch after every replay notification, not after a guessed message count.
                        boundary.send(None).map_err(Error::into_internal_error)?;
                        let _ = done.send(result);
                        Ok(())
                    })?;
                    let attached = response.await.map_err(Error::into_internal_error)??;
                    assert_eq!(attached.meta.as_ref().unwrap()["hive"]["running"], false);
                    let mut messages = Vec::new();
                    while let Some(event) = rx
                        .recv()
                        .await
                        .expect("Replay connection ended before response boundary")
                    {
                        assert_ne!(
                            event["unexpectedPermission"], true,
                            "replay must not request permission again"
                        );
                        assert!(
                            event["sessionId"] == session.to_string(),
                            "replay crossed session identities"
                        );
                        let update = &event["update"];
                        match update["sessionUpdate"].as_str() {
                            Some("user_message" | "agent_message") => messages.push(update.clone()),
                            Some("state_update") => assert_eq!(update["state"], "idle"),
                            Some("user_message_chunk" | "agent_message_chunk") => panic!(
                                "replay must restore snapshots without starting another turn"
                            ),
                            _ => {}
                        }
                        assert!(
                            messages.len() <= 32,
                            "Replay exceeded the short probe transcript budget"
                        );
                    }
                    let ids: std::collections::BTreeSet<_> = messages
                        .iter()
                        .map(|message| {
                            (
                                message["sessionUpdate"].to_string(),
                                message["messageId"].to_string(),
                            )
                        })
                        .collect();
                    assert_eq!(
                        ids.len(),
                        messages.len(),
                        "replay duplicated message identities"
                    );
                    Ok(messages)
                },
            ),
    )
    .await
    .map_err(|_| "transcript replay timed out".to_string())?
    .map_err(|_| "transcript replay failed; inspect session history and agent capabilities".into())
}

async fn lifecycle(address: SocketAddr, id: &str, database: PathBuf) -> Result<(), String> {
    let cwd = workspace(id)?;
    let providers: serde_json::Value = reqwest::get(format!("http://{address}/v1/providers"))
        .await
        .map_err(|_| "Hive providers request failed")?
        .json()
        .await
        .map_err(|_| "invalid Hive providers response")?;
    if providers["agents"][0]["id"] != id || providers["agents"][0]["transport"] != "stdio" {
        return Err("Hive did not register the requested stdio profile".into());
    }
    let (tx, mut rx) = mpsc::unbounded_channel();
    let original_database = database.clone();
    let first = Client.v2().on_receive_notification(
        async move |update: v2::UpdateSessionNotification, _: V2ConnectionTo<Agent>| {
            tx.send(update).map_err(Error::into_internal_error)
        },
        agent_client_protocol::on_receive_notification!(),
    ).connect_with(HttpClient::new(format!("ws://{address}")).map_err(|_| "invalid WS URL")?, async move |cx| {
        let initialized = cx.send_request(v2::InitializeRequest::new(
            ProtocolVersion::V2, v2::Implementation::new("real-agent-probe", "1"),
        )).block_task().await.map_err(|_| diagnostic("Hive initialize failed"))?;
        if initialized.protocol_version != ProtocolVersion::V2 || initialized.capabilities.session.is_none() {
            return Err(diagnostic("Hive did not advertise v2 session support"));
        }
        let mut meta = v2::Meta::new();
        meta.insert("hive".into(), serde_json::json!({"agent": id}));
        let opened = cx.send_request(v2::NewSessionRequest::new(cwd.clone()).meta(meta))
            .block_task().await.map_err(|_| diagnostic("real agent session/new failed; inspect wire version, ACP entrypoint and agent configuration"))?;
        let session = opened.session_id;
        let observed = turn(&cx, &mut rx, &session, &opened.meta, "real-agent-first").await.map_err(|stage| diagnostic(&stage))?;
        if !observed.text { return Err(diagnostic("first turn completed without observable assistant text")); }
        eprintln!("FIRST running={} chunks={} snapshots={} tools={} idle=true text=true", observed.running, observed.chunks, observed.snapshots, observed.tools);
        let replay = if id == "opencode" {
            let second = turn(&cx, &mut rx, &session, &opened.meta, "real-agent-before-reload").await.map_err(|stage| diagnostic(&stage))?;
            assert!(second.text, "second turn must complete before reload");
            let snapshot = replay_snapshot(address, "ws", session.clone(), cwd.clone()).await.map_err(|_| diagnostic("Hive live-worker session/resume failed"))?;
            assert_eq!(snapshot.len(), 4, "both completed turns must be retained");
            for (pair, answer) in snapshot.as_chunks::<2>().0.iter().zip([observed.answer, second.answer]) {
                assert_eq!(pair[0]["sessionUpdate"], "user_message");
                assert!(pair[0]["content"][0]["text"] == PROBE_PROMPT, "restored user content must match the submitted prompt");
                assert_eq!(pair[1]["sessionUpdate"], "agent_message");
                let text: String = pair[1]["content"].as_array().unwrap().iter().filter_map(|block| block["text"].as_str()).collect();
                assert!(text == answer, "restored output must match the actual completed turn");
            }
            Some(snapshot)
        } else { None };
        let identity = saved_identity(&original_database, &session);
        Ok((session, cwd, replay, identity))
    }).await.map_err(stage)?;
    if let Some(expected) = &first.2 {
        let identity = &first.3;
        assert!(!identity["nativeId"].as_str().unwrap().is_empty());
        assert!(
            identity["cwd"] == first.1.to_str().unwrap(),
            "saved workspace changed"
        );
        for scheme in ["http", "ws", "http"] {
            let replay = replay_snapshot(address, scheme, first.0.clone(), first.1.clone()).await?;
            assert!(
                &replay == expected,
                "fresh-client replay changed or duplicated retained history"
            );
            assert!(
                &saved_identity(&database, &first.0) == identity,
                "reopening changed native identity or workspace"
            );
        }
        eprintln!(
            "REPLAY agent={id} turns=2 reloads=3 ordered=true duplicates=false identity_preserved=true boundary=live-worker"
        );
    }
    let status: serde_json::Value = reqwest::get(format!("http://{address}/v1/status"))
        .await
        .map_err(|_| "Hive status request failed")?
        .json()
        .await
        .map_err(|_| "invalid Hive status response")?;
    if status["sessions"] != 1 {
        return Err("client disconnect lost the live Hive session".into());
    }
    let (tx, mut rx) = mpsc::unbounded_channel();
    Client
        .v2()
        .on_receive_notification(
            async move |update: v2::UpdateSessionNotification, _: V2ConnectionTo<Agent>| {
                tx.send(update).map_err(Error::into_internal_error)
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(
            HttpClient::new(format!("http://{address}")).map_err(|_| "invalid HTTP URL")?,
            async move |cx| {
                cx.send_request(v2::InitializeRequest::new(
                    ProtocolVersion::V2,
                    v2::Implementation::new("real-agent-probe", "1"),
                ))
                .block_task()
                .await
                .map_err(|_| diagnostic("second Hive initialize failed"))?;
                let (session, cwd, _, _) = first;
                let listed = cx
                    .send_request(v2::ListSessionsRequest::new())
                    .block_task()
                    .await
                    .map_err(|_| diagnostic("Hive session/list failed"))?;
                if !listed
                    .sessions
                    .iter()
                    .any(|entry| entry.session_id == session)
                {
                    return Err(diagnostic(
                        "Hive did not list its disconnected live session",
                    ));
                }
                let resumed = cx
                    .send_request(v2::ResumeSessionRequest::new(session.clone(), cwd))
                    .block_task()
                    .await
                    .map_err(|_| diagnostic("Hive live-worker session/resume failed"))?;
                let observed = turn(&cx, &mut rx, &session, &resumed.meta, "real-agent-second")
                    .await
                    .map_err(|stage| diagnostic(&stage))?;
                if !observed.text {
                    return Err(diagnostic(
                        "resumed turn completed without observable assistant text",
                    ));
                }
                eprintln!(
                    "RESUMED running={} chunks={} snapshots={} tools={} idle=true text=true",
                    observed.running, observed.chunks, observed.snapshots, observed.tools
                );
                cx.send_request(
                    v2::CloseSessionRequest::new(session).meta(owned(&resumed.meta, "")),
                )
                .block_task()
                .await
                .map_err(|_| diagnostic("Hive session/close failed; check agent close support"))?;
                Ok(())
            },
        )
        .await
        .map_err(stage)?;
    let status: serde_json::Value = reqwest::get(format!("http://{address}/v1/status"))
        .await
        .map_err(|_| "Hive final status request failed")?
        .json()
        .await
        .map_err(|_| "invalid Hive final status response")?;
    if status["sessions"] != 0 {
        return Err("closed session remains in Hive's active status".into());
    }
    Ok(())
}

async fn native_session(installation: Installation) -> Result<(), String> {
    let key = format!(
        "HIVE_REAL_{}_NATIVE_ID",
        installation.id.to_ascii_uppercase()
    );
    let Ok(native_id) = std::env::var(&key) else {
        eprintln!(
            "SKIP {} native attachment: create a native session first, then set {key}",
            installation.id
        );
        return Ok(());
    };
    if native_id.trim().is_empty() {
        return Err(format!("{key} must be a nonempty native session ID"));
    }
    let cwd = workspace(installation.id)?;
    let agent = AcpAgent::new(AcpAgentConfig::new(&installation.command).args(installation.args));
    let replayed = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let received = replayed.clone();
    let result = if installation.wire == AgentVersion::V1 {
        Client.builder().on_receive_notification(
            async move |update: v1::SessionNotification, _: ConnectionTo<Agent>| {
                if matches!(update.update, v1::SessionUpdate::UserMessageChunk(_) | v1::SessionUpdate::AgentMessageChunk(_)) {
                    received.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        ).connect_with(agent, async move |cx| {
            let initialized = cx.send_request(v1::InitializeRequest::new(ProtocolVersion::V1))
                .block_task().await.map_err(|_| diagnostic("native v1 initialize failed"))?;
            if initialized.protocol_version != ProtocolVersion::V1 {
                return Err(diagnostic("native agent did not negotiate ACPv1"));
            }
            if !initialized.agent_capabilities.load_session {
                return Err(diagnostic("native agent does not advertise history restoration via session/load"));
            }
            eprintln!("NATIVE_CAPABILITIES wire=v1 load_session=true");
            cx.send_request(v1::LoadSessionRequest::new(native_id, cwd))
                .block_task().await.map_err(|_| diagnostic("session/load rejected native ID; check cwd and native ACP load capability"))?;
            Ok(())
        }).await.map_err(stage)
    } else {
        Client.v2().on_receive_notification(
            async move |update: v2::UpdateSessionNotification, _: V2ConnectionTo<Agent>| {
                if matches!(update.update, v2::SessionUpdate::UserMessage(_) | v2::SessionUpdate::AgentMessage(_) | v2::SessionUpdate::UserMessageChunk(_) | v2::SessionUpdate::AgentMessageChunk(_)) {
                    received.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        ).connect_with(agent, async move |cx| {
            let initialized = cx.send_request(v2::InitializeRequest::new(
                ProtocolVersion::V2, v2::Implementation::new("real-native-probe", "1"),
            )).block_task().await.map_err(|_| diagnostic("native v2 initialize failed"))?;
            if initialized.protocol_version != ProtocolVersion::V2 {
                return Err(diagnostic("native agent did not negotiate ACPv2"));
            }
            if initialized.capabilities.session.is_none() {
                return Err(diagnostic("native agent does not advertise session support for history restoration"));
            }
            eprintln!("NATIVE_CAPABILITIES wire=v2 session=true replay_from=start");
            cx.send_request(v2::ResumeSessionRequest::new(v2::SessionId::new(native_id), cwd).replay_from(v2::ReplayFrom::Start(v2::ReplayFromStart::new())))
                .block_task().await.map_err(|_| diagnostic("session/resume rejected native ID; check cwd and native ACP resume capability"))?;
            Ok(())
        }).await.map_err(stage)
    };
    result?;
    let replayed = replayed.load(std::sync::atomic::Ordering::Relaxed);
    if replayed == 0 {
        return Err("native attachment replayed no messages; select a retained session with a nonempty conversation".into());
    }
    eprintln!(
        "NATIVE agent={} version={} wire={:?} stdio attachment=true replayed_messages={} (direct ACP; Hive adoption unimplemented)",
        installation.id, installation.version, installation.wire, replayed
    );
    Ok(())
}

#[tokio::test]
#[ignore = "explicit opt-in: starts a locally installed agent; sends no prompt, so incurs no model costs"]
async fn pi_initialize_probe_reports_a_capability_report() {
    let Some(installation) = installation("pi").await else {
        return;
    };
    let profile = Profile {
        name: None,
        command: Some(installation.command),
        args: installation.args,
        env: BTreeMap::new(),
        endpoint: None,
        headers: BTreeMap::new(),
        versions: vec![installation.wire],
        pricing: Default::default(),
        kind: Default::default(),
        username: None,
        password_env: None,
    };
    match hive_daemon::compat::probe_initialize(&profile).await {
        Ok(report) => eprintln!(
            "CAPABILITIES pi_version={} {}",
            installation.version,
            report.summary()
        ),
        // Print the diagnostic code and help only: both are Hive-authored, never agent output.
        Err(error) => {
            let code = miette::Diagnostic::code(&error).map(|code| code.to_string());
            let help = miette::Diagnostic::help(&error).map(|help| help.to_string());
            panic!(
                "Pi {} probe failed: {error} [{code:?}] {help:?}",
                installation.version
            );
        }
    }
}

#[tokio::test]
#[ignore = "explicit opt-in: starts a locally installed agent and may incur model costs"]
async fn pi_creates_prompts_and_reattaches_to_a_live_hive_session() {
    let Some(installation) = installation("pi").await else {
        return;
    };
    let version = installation.version.clone();
    let wire = installation.wire;
    if let Err(stage) = run(installation).await {
        panic!("Pi {version} stdio {wire:?}: {stage}");
    }
}

#[tokio::test]
async fn the_opencode_replay_probe_checks_complete_history_with_a_deterministic_agent() {
    // Exercise the opt-in probe's assertions in ordinary CI without relying on
    // credentials or model output; this is harness evidence, not OpenCode certification.
    for wire in [AgentVersion::V1, AgentVersion::V2] {
        run(Installation {
            id: "opencode",
            command: env!("CARGO_BIN_EXE_hive-test-agent").into(),
            args: if wire == AgentVersion::V1 {
                vec!["v1".into()]
            } else {
                Vec::new()
            },
            version: "deterministic-fixture".into(),
            wire,
        })
        .await
        .unwrap();
    }
}

#[tokio::test]
#[ignore = "explicit opt-in: starts a locally installed agent and may incur model costs"]
async fn opencode_creates_prompts_and_reattaches_to_a_live_hive_session() {
    let Some(installation) = installation("opencode").await else {
        return;
    };
    let version = installation.version.clone();
    let wire = installation.wire;
    if let Err(stage) = run(installation).await {
        panic!("OpenCode {version} stdio {wire:?}: {stage}");
    }
}

#[tokio::test]
#[ignore = "requires explicit opt-in and an existing native Pi session ID"]
async fn pi_reports_whether_a_native_session_can_be_attached_over_acp() {
    let Some(installation) = installation("pi").await else {
        return;
    };
    let version = installation.version.clone();
    let wire = installation.wire;
    match timeout(Duration::from_secs(30), native_session(installation)).await {
        Ok(Ok(())) => {}
        Ok(Err(stage)) => panic!("Pi {version} stdio {wire:?}: {stage}"),
        Err(_) => panic!("Pi {version} stdio {wire:?}: native attachment timed out"),
    }
}

#[tokio::test]
#[ignore = "requires explicit opt-in and an existing native OpenCode session ID"]
async fn opencode_reports_whether_a_native_session_can_be_attached_over_acp() {
    let Some(installation) = installation("opencode").await else {
        return;
    };
    let version = installation.version.clone();
    let wire = installation.wire;
    match timeout(Duration::from_secs(30), native_session(installation)).await {
        Ok(Ok(())) => {}
        Ok(Err(stage)) => panic!("OpenCode {version} stdio {wire:?}: {stage}"),
        Err(_) => panic!("OpenCode {version} stdio {wire:?}: native attachment timed out"),
    }
}
