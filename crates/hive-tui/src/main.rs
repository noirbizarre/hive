//! Minimal interactive terminal client over the public ACPv2 endpoint.

use agent_client_protocol::{
    Agent, Client, Error, V2ConnectionTo,
    schema::{ProtocolVersion, v2},
};
use clap::Parser;
use hive_protocol::{HttpClient, daemon_status};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    sync::mpsc,
};

#[derive(Parser)]
struct Options {
    #[arg(long, default_value = "127.0.0.1:8765")]
    address: String,
    #[arg(long, default_value = "ws", value_parser = ["ws", "http"])]
    transport: String,
    #[arg(
        long,
        help = "Agent profile ID (defaults to the daemon's configured default)"
    )]
    agent: Option<String>,
    #[arg(long, help = "Take control of an existing Hive session ID")]
    session: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let options = Options::parse();
    let status = daemon_status(&format!("http://{}", options.address)).await?;
    println!("Hive: {status}");
    let url = format!("{}://{}", options.transport, options.address);
    let (tx, mut rx) = mpsc::unbounded_channel();
    Client
        .v2()
        .on_receive_notification(
            async move |update: v2::UpdateSessionNotification, _cx: V2ConnectionTo<Agent>| {
                tx.send(update).map_err(Error::into_internal_error)
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(HttpClient::new(url)?, async move |cx| {
            let init = cx
                .send_request(v2::InitializeRequest::new(
                    ProtocolVersion::V2,
                    v2::Implementation::new("hive-tui", "0.1.0"),
                ))
                .block_task()
                .await?;
            if init.capabilities.session.is_none() {
                return Err(Error::invalid_request().data("Hive did not advertise session support"));
            }
            println!("ACPv2 connected; type a prompt, or /quit");
            let cwd = std::env::current_dir().map_err(Error::into_internal_error)?;
            let (id, owner) = if let Some(id) = &options.session {
                let id = v2::SessionId::new(id.clone());
                let response = cx
                    .send_request(
                        v2::ResumeSessionRequest::new(id.clone(), cwd)
                            .replay_from(v2::ReplayFrom::Start(v2::ReplayFromStart::new())),
                    )
                    .block_task()
                    .await?;
                (id, response.meta)
            } else {
                let mut request = v2::NewSessionRequest::new(cwd);
                if let Some(agent) = &options.agent {
                    let mut meta = v2::Meta::new();
                    meta.insert("hive".into(), serde_json::json!({ "agent": agent }));
                    request = request.meta(meta);
                }
                let response = cx.send_request(request).block_task().await?;
                (response.session_id, response.meta)
            };
            let owner = owner
                .and_then(|meta| meta.get("hive").cloned())
                .ok_or_else(|| {
                    Error::invalid_request().data("Hive did not return controller metadata")
                })?;
            println!("Hive session: {id}; use --session {id} to take control after exit");
            let mut last_seq = 0u64;
            // Resume sends replay before its response; render that backlog before a new turn.
            let boundary = if options.session.is_some() {
                owner["nextSeq"].as_u64().unwrap_or(1)
            } else {
                1
            };
            while last_seq + 1 < boundary {
                let update = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
                    .await
                    .map_err(Error::into_internal_error)?
                    .ok_or_else(Error::internal_error)?;
                if update.session_id != id {
                    continue;
                }
                if let Some(seq) = update
                    .meta
                    .as_ref()
                    .and_then(|m| m.get("hive"))
                    .and_then(|h| h.get("seq"))
                    .and_then(serde_json::Value::as_u64)
                {
                    if seq <= last_seq {
                        continue;
                    }
                    last_seq = seq;
                }
                if let v2::SessionUpdate::AgentMessageChunk(chunk) = update.update {
                    println!("{chunk:?}");
                }
            }
            if owner["running"] == true {
                println!("Agent is still working; waiting for idle before another prompt");
                while let Some(update) = rx.recv().await {
                    if update.session_id != id {
                        continue;
                    }
                    if let Some(seq) = update
                        .meta
                        .as_ref()
                        .and_then(|m| m.get("hive"))
                        .and_then(|h| h.get("seq"))
                        .and_then(serde_json::Value::as_u64)
                    {
                        if seq <= last_seq {
                            continue;
                        }
                        last_seq = seq;
                    }
                    match update.update {
                        v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(_)) => break,
                        v2::SessionUpdate::AgentMessageChunk(chunk) => println!("{chunk:?}"),
                        _ => {}
                    }
                }
            }
            let mut stdin = BufReader::new(tokio::io::stdin()).lines();
            while let Some(line) = stdin
                .next_line()
                .await
                .map_err(Error::into_internal_error)?
            {
                if line.trim() == "/quit" {
                    break;
                }
                if line.trim().is_empty() {
                    continue;
                }
                let mut meta = v2::Meta::new();
                let mut hive = owner.clone();
                hive["key"] = serde_json::json!(uuid::Uuid::new_v4().to_string());
                meta.insert("hive".into(), hive);
                cx.send_request(v2::PromptRequest::new(id.clone(), vec![line.into()]).meta(meta))
                    .block_task()
                    .await?;
                let mut running = false;
                while let Some(update) = rx.recv().await {
                    if update.session_id != id {
                        continue;
                    }
                    if let Some(seq) = update
                        .meta
                        .as_ref()
                        .and_then(|m| m.get("hive"))
                        .and_then(|h| h.get("seq"))
                        .and_then(serde_json::Value::as_u64)
                    {
                        if seq <= last_seq {
                            continue;
                        }
                        last_seq = seq;
                    }
                    match update.update {
                        v2::SessionUpdate::StateUpdate(v2::StateUpdate::Running(_)) => {
                            running = true
                        }
                        v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(_)) if running => {
                            break;
                        }
                        v2::SessionUpdate::AgentMessageChunk(chunk) => println!("{chunk:?}"),
                        _ => {}
                    }
                }
            }
            Ok(())
        })
        .await?;
    Ok(())
}
