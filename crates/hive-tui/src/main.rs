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
            let session = cx
                .build_session_cwd()?
                .start_session()
                .block_task()
                .await?
                .into_session();
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
                session.send_prompt(line).block_task().await?;
                let mut running = false;
                while let Some(update) = rx.recv().await {
                    if update.session_id != *session.session_id() {
                        continue;
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
            session.close().block_task().await?;
            Ok(())
        })
        .await?;
    Ok(())
}
