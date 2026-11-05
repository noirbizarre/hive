//! Supervision and MCP wire adaptation; request correlation remains in the ACP SDK.

use std::{io, path::PathBuf, process::Stdio, sync::Arc};

use agent_client_protocol::{
    ConnectionTo, Lines, RawJsonRpcMessage, Responder, UntypedMessage,
    role::mcp::{Client, Server},
};
use command_group::{AsyncCommandGroup, AsyncGroupChild};
use futures::{SinkExt, StreamExt};
use hive_core::{
    identity::NodeId,
    workspace::{NodePath, PathSyntax},
};
use serde_json::{Value, json};
use tokio::{process::Command, sync::watch};
use tokio_util::codec::{FramedRead, FramedWrite, LinesCodec};
use tracing::instrument::WithSubscriber;

use super::{
    InstanceIdentity, Lifecycle, Ownership, ResolvedTransport,
    runtime::{Instance, Ready, RuntimeError, Status},
};

struct ProcessGroup(AsyncGroupChild, bool);

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        // Tokio task abortion must kill descendants too, not only the process leader.
        if self.1 {
            let _ = self.0.start_kill();
        }
    }
}

fn host_path(path: &NodePath, node: &NodeId) -> Result<PathBuf, RuntimeError> {
    let syntax = if cfg!(windows) {
        PathSyntax::Windows
    } else {
        PathSyntax::Posix
    };
    let native = PathBuf::from(path.as_str());
    if path.node() != node || path.syntax() != syntax || !native.is_absolute() {
        return Err(RuntimeError::Inputs);
    }
    Ok(native)
}

pub(super) fn validate(identity: &InstanceIdentity, node: &NodeId) -> Result<(), RuntimeError> {
    if identity.definition.ownership() != Ownership::Managed {
        return Err(RuntimeError::Inputs);
    }
    if let Lifecycle::OnDemand {
        idle_timeout_seconds,
    } = identity.definition.lifecycle()
        && tokio::time::Instant::now()
            .checked_add(std::time::Duration::from_secs(*idle_timeout_seconds))
            .is_none()
    {
        return Err(RuntimeError::Inputs);
    }
    let ResolvedTransport::Stdio {
        executable, cwd, ..
    } = &identity.transport
    else {
        return Err(RuntimeError::Inputs);
    };
    host_path(executable, node)?;
    host_path(cwd, node)?;
    let init = &identity.initialization;
    if init.protocol_version != "2025-11-25"
        || !init.client_info.is_object()
        || !init.client_info.get("name").is_some_and(Value::is_string)
        || !init
            .client_info
            .get("version")
            .is_some_and(Value::is_string)
        || !init.capabilities.is_object()
        || !init.options.is_object()
    {
        return Err(RuntimeError::Inputs);
    }
    for (key, value) in init.capabilities.as_object().ok_or(RuntimeError::Inputs)? {
        if key != "roots"
            || !value.is_object()
            || !value.as_object().is_some_and(|value| value.is_empty())
        {
            return Err(RuntimeError::Inputs);
        }
    }
    if ["protocolVersion", "clientInfo", "capabilities"]
        .iter()
        .any(|key| init.options.get(key).is_some())
    {
        return Err(RuntimeError::Inputs);
    }
    for root in &init.roots {
        host_path(root, node)?;
        url::Url::from_file_path(root.as_str()).map_err(|_| RuntimeError::Inputs)?;
    }
    Ok(())
}

pub(super) async fn run(
    instance: Arc<Instance>,
    status: watch::Sender<Status>,
    mut stop: watch::Receiver<bool>,
) -> Option<RuntimeError> {
    if *stop.borrow() {
        return None;
    }
    let ResolvedTransport::Stdio {
        executable,
        args,
        env,
        cwd,
    } = &instance.identity.transport
    else {
        return Some(RuntimeError::Inputs);
    };
    let mut command = Command::new(executable.as_str());
    command
        .args(args)
        .env_clear()
        .envs(env)
        .current_dir(cwd.as_str())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = match command.group_spawn() {
        Ok(child) => child,
        Err(_) => return Some(RuntimeError::Startup),
    };
    let mut group = ProcessGroup(child, true);
    let child = &mut group.0;
    let stdin = child.inner().stdin.take().expect("piped stdin");
    let stdout = child.inner().stdout.take().expect("piped stdout");
    let mut stderr = child.inner().stderr.take().expect("piped stderr");
    // Never let diagnostics fill a pipe or mix with line-delimited protocol output.
    let diagnostics = tokio::spawn(async move {
        let _ = tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await;
    });
    let max_frame = instance.policy.max_frame_bytes;
    let outgoing = SinkExt::<String>::sink_map_err(
        FramedWrite::new(stdin, LinesCodec::new_with_max_length(max_frame)),
        io::Error::other,
    )
    .with(move |line: String| async move {
        if line.len() > max_frame {
            return Err(io::Error::other("MCP frame too large"));
        }
        // The generic SDK cancellation API speaks ACP; this transport speaks MCP.
        let mut message: Value = serde_json::from_str(&line).map_err(io::Error::other)?;
        if message.get("method").and_then(Value::as_str) == Some("$/cancel_request") {
            message["method"] = json!("notifications/cancelled");
        }
        serde_json::to_string(&message).map_err(io::Error::other)
    });
    let incoming =
        FramedRead::new(stdout, LinesCodec::new_with_max_length(max_frame)).map(|line| {
            let line = line.map_err(io::Error::other)?;
            // The SDK drops malformed responses; fail the transport so pending calls cannot hang.
            // Use its envelope parser to keep admission aligned without duplicating routing.
            serde_json::from_str::<RawJsonRpcMessage>(&line).map_err(|_| {
                // Parser errors can include peer-controlled values; never expose payloads.
                io::Error::new(io::ErrorKind::InvalidData, "invalid MCP envelope")
            })?;
            Ok(line)
        });
    let transport = Lines::new(outgoing, incoming);
    let init = instance.identity.initialization.clone();
    let roots: Vec<Value> = init
        .roots
        .iter()
        .filter_map(|root| {
            url::Url::from_file_path(root.as_str())
                .ok()
                .map(|uri| json!({"uri": uri.as_str()}))
        })
        .collect();
    let roots_supported = init.capabilities.get("roots").is_some();
    let startup_timeout = instance.policy.startup_timeout;
    let (initialized, mut initialization) = watch::channel(false);
    let connection = Client
        .builder()
        .on_receive_request(
            async move |request: UntypedMessage,
                        responder: Responder<Value>,
                        _cx: ConnectionTo<Server>| {
                match request.method.as_str() {
                    "ping" => responder.respond(json!({})),
                    "roots/list" if roots_supported => responder.respond(json!({"roots": roots})),
                    _ => responder
                        .respond_with_error(agent_client_protocol::Error::method_not_found()),
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |_notification: UntypedMessage, _cx: ConnectionTo<Server>| Ok(()),
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(transport, async move |cx: ConnectionTo<Server>| {
            let mut params = init.options;
            params["protocolVersion"] = json!(init.protocol_version);
            params["clientInfo"] = init.client_info;
            params["capabilities"] = init.capabilities;
            let response = cx
                .send_request(UntypedMessage {
                    method: "initialize".into(),
                    params,
                })
                .block_task()
                .await?;
            if response.get("protocolVersion").and_then(Value::as_str) != Some("2025-11-25")
                || !response.get("capabilities").is_some_and(Value::is_object)
                || !response
                    .get("serverInfo")
                    .and_then(|info| info.get("name"))
                    .is_some_and(Value::is_string)
                || !response
                    .get("serverInfo")
                    .and_then(|info| info.get("version"))
                    .is_some_and(Value::is_string)
            {
                return Err(agent_client_protocol::Error::invalid_params());
            }
            cx.send_notification(UntypedMessage {
                method: "notifications/initialized".into(),
                params: json!({}),
            })?;
            status.send_replace(Status::Ready(Ready {
                connection: cx.clone(),
                capabilities: response["capabilities"].clone(),
                since: tokio::time::Instant::now(),
            }));
            initialized.send_replace(true);
            // Clean EOF alone does not end an SDK foreground future; end it explicitly.
            cx.incoming_closed().await;
            Ok(())
        });
    // SDK debug events include payloads. Scope suppression to this connection driver.
    let mut connection =
        Box::pin(connection.with_subscriber(tracing::subscriber::NoSubscriber::default()));
    let startup = tokio::time::sleep(startup_timeout);
    tokio::pin!(startup);
    let error = loop {
        tokio::select! {
            biased;
            _ = stop.changed() => { break None; }
            _ = child.inner().wait() => { break Some(RuntimeError::Connection); }
            _ = &mut connection => { break Some(if *initialization.borrow() { RuntimeError::Connection } else { RuntimeError::Initialize }); }
            _ = &mut startup, if !*initialization.borrow() => { break Some(RuntimeError::Initialize); }
            result = initialization.changed(), if !*initialization.borrow() => { if result.is_err() { break Some(RuntimeError::Initialize); } }
        }
    };
    // Drop the driver before waiting so stdin closes for cooperative stdio shutdown.
    drop(connection);
    if error.is_none() {
        let _ = tokio::time::timeout(instance.policy.exit_grace, child.inner().wait()).await;
    }
    // Killing the group also covers descendants retaining protocol pipes after leader exit.
    let _ = child.start_kill();
    let reaped = matches!(
        tokio::time::timeout(instance.policy.exit_grace, child.wait()).await,
        Ok(Ok(_))
    );
    group.1 = !reaped;
    diagnostics.abort();
    if reaped {
        error
    } else {
        Some(RuntimeError::Cleanup)
    }
}
