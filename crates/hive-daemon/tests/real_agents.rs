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

struct Installation {
    id: &'static str,
    command: String,
    args: Vec<String>,
    version: String,
    wire: AgentVersion,
}

async fn installation(id: &'static str) -> Option<Installation> {
    let prefix = format!("HIVE_REAL_{}", id.to_ascii_uppercase());
    if std::env::var(&prefix).as_deref() != Ok("1") {
        eprintln!("SKIP {id}: set {prefix}=1 and run this ignored test explicitly");
        return None;
    }
    let command = std::env::var(format!("{prefix}_COMMAND")).unwrap_or_else(|_| id.into());
    let args = match std::env::var(format!("{prefix}_ARGS_JSON")) {
        Ok(value) => serde_json::from_str::<Vec<String>>(&value)
            .expect("ARGS_JSON must be a JSON array of separate argument strings"),
        Err(_) if id == "opencode" => vec!["acp".into()],
        Err(_) => {
            eprintln!(
                "SKIP {id}: set {prefix}_ARGS_JSON to a verified ACP entrypoint (Pi's installed CLI does not advertise one)"
            );
            return None;
        }
    };
    let wire = match std::env::var(format!("{prefix}_WIRE")) {
        Ok(value) if value == "v2" => AgentVersion::V2,
        Ok(value) if value == "v1" => AgentVersion::V1,
        Err(_) if id == "opencode" => AgentVersion::V1,
        _ => panic!("{prefix}_WIRE must be v1 or v2; verify the agent's actual ACP protocol"),
    };
    let result = timeout(
        Duration::from_secs(5),
        tokio::process::Command::new(&command)
            .arg("--version")
            .stderr(Stdio::null())
            .output(),
    )
    .await;
    let output = match result {
        Ok(Ok(output)) if output.status.success() => output,
        Ok(Err(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            eprintln!(
                "SKIP {id}: executable unavailable; set {prefix}_COMMAND to an installed binary"
            );
            return None;
        }
        _ => {
            eprintln!(
                "SKIP {id}: product version unavailable; verify {prefix}_COMMAND and its --version support"
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
    let request = v2::PromptRequest::new(
        id.clone(),
        vec!["Reply with one short sentence about the weather on Mars. Do not use tools or read files.".into()],
    ).meta(owned(owner, key));
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
                    }
                }
                v2::SessionUpdate::AgentMessage(message) => {
                    observed.snapshots = true;
                    if let MaybeUndefined::Value(content) = message.content {
                        observed.text |= content.iter().any(|content| {
                            matches!(content, v2::ContentBlock::Text(text) if !text.text.trim().is_empty())
                        });
                    }
                }
                v2::SessionUpdate::ToolCallUpdate(_) => observed.tools = true,
                v2::SessionUpdate::StateUpdate(v2::StateUpdate::Running(_)) => observed.running = true,
                v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(_)) => return Ok(observed),
                _ => {}
            }
        }
        Err("agent connection ended before idle".into())
    }).await.map_err(|_| "no idle update before turn timeout; cancel or inspect agent logs".to_string())?
}

async fn run(installation: Installation) -> Result<(), String> {
    let profile = Profile {
        name: None,
        command: Some(installation.command),
        args: installation.args,
        env: BTreeMap::new(),
        endpoint: None,
        headers: BTreeMap::new(),
        versions: vec![installation.wire],
        pricing: Default::default(),
    };
    let mut agents = BTreeMap::new();
    agents.insert(installation.id.into(), profile);
    let broker = hive_daemon::Broker::new(Config {
        default_agent: Some(installation.id.into()),
        agents,
    });
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
    let mut flow = tokio::spawn(lifecycle(address, installation.id));
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

async fn lifecycle(address: SocketAddr, id: &str) -> Result<(), String> {
    let cwd = workspace(id)?;
    let providers: serde_json::Value = reqwest::get(format!("http://{address}/hive/v1/providers"))
        .await
        .map_err(|_| "Hive providers request failed")?
        .json()
        .await
        .map_err(|_| "invalid Hive providers response")?;
    if providers["agents"][0]["id"] != id || providers["agents"][0]["transport"] != "stdio" {
        return Err("Hive did not register the requested stdio profile".into());
    }
    let (tx, mut rx) = mpsc::unbounded_channel();
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
        Ok((session, cwd))
    }).await.map_err(stage)?;
    let status: serde_json::Value = reqwest::get(format!("http://{address}/hive/v1/status"))
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
                let (session, cwd) = first;
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
    let status: serde_json::Value = reqwest::get(format!("http://{address}/hive/v1/status"))
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
    let result = if installation.wire == AgentVersion::V1 {
        Client.builder().on_receive_notification(
            async |_update: v1::SessionNotification, _: ConnectionTo<Agent>| Ok(()),
            agent_client_protocol::on_receive_notification!(),
        ).connect_with(agent, async move |cx| {
            let initialized = cx.send_request(v1::InitializeRequest::new(ProtocolVersion::V1))
                .block_task().await.map_err(|_| diagnostic("native v1 initialize failed"))?;
            if initialized.protocol_version != ProtocolVersion::V1 {
                return Err(diagnostic("native agent did not negotiate ACPv1"));
            }
            cx.send_request(v1::LoadSessionRequest::new(native_id, cwd))
                .block_task().await.map_err(|_| diagnostic("session/load rejected native ID; check cwd and native ACP load capability"))?;
            Ok(())
        }).await.map_err(stage)
    } else {
        Client.v2().on_receive_notification(
            async |_update: v2::UpdateSessionNotification, _: V2ConnectionTo<Agent>| Ok(()),
            agent_client_protocol::on_receive_notification!(),
        ).connect_with(agent, async move |cx| {
            let initialized = cx.send_request(v2::InitializeRequest::new(
                ProtocolVersion::V2, v2::Implementation::new("real-native-probe", "1"),
            )).block_task().await.map_err(|_| diagnostic("native v2 initialize failed"))?;
            if initialized.protocol_version != ProtocolVersion::V2 {
                return Err(diagnostic("native agent did not negotiate ACPv2"));
            }
            cx.send_request(v2::ResumeSessionRequest::new(v2::SessionId::new(native_id), cwd))
                .block_task().await.map_err(|_| diagnostic("session/resume rejected native ID; check cwd and native ACP resume capability"))?;
            Ok(())
        }).await.map_err(stage)
    };
    result?;
    eprintln!(
        "NATIVE agent={} version={} wire={:?} stdio attachment=true (direct ACP; Hive adoption unimplemented)",
        installation.id, installation.version, installation.wire
    );
    Ok(())
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
