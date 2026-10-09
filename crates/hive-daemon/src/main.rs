use std::net::SocketAddr;

use clap::{Parser, Subcommand};
use hive_protocol::{AgentVersion, StdioProvider};

#[derive(Parser)]
#[command(name = "hive", about = "Local ACPv2 gateway for coding agents")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Serve {
        #[arg(long, default_value = "127.0.0.1:8765", env = "HIVE_LISTEN")]
        listen: SocketAddr,
        #[arg(long, env = "HIVE_AGENT")]
        agent: String,
        #[arg(long, default_value = "v2", value_parser = ["v1", "v2"])]
        agent_version: String,
    },
    Status {
        #[arg(long, default_value = "http://127.0.0.1:8765", env = "HIVE_URL")]
        url: String,
    },
}

#[tokio::main]
async fn main() -> miette::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    match Cli::parse().command {
        Command::Serve {
            listen,
            agent,
            agent_version,
        } => {
            let version = if agent_version == "v1" {
                AgentVersion::V1
            } else {
                AgentVersion::V2
            };
            hive_daemon::serve(listen, StdioProvider::new(agent, version))
                .await
                .map_err(|e| miette::miette!("cannot serve Hive on {listen}: {e}"))?;
        }
        Command::Status { url } => {
            let url = format!("{}/hive/v1/status", url.trim_end_matches('/'));
            let response = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(3))
                .build()
                .map_err(|e| miette::miette!("HTTP client: {e}"))?
                .get(&url)
                .send()
                .await
                .map_err(|e| {
                    miette::miette!(
                        "daemon unavailable at {url}: {e}; start `hive serve --agent <command>`"
                    )
                })?
                .error_for_status()
                .map_err(|e| miette::miette!("daemon status failed: {e}"))?;
            println!(
                "{}",
                response
                    .text()
                    .await
                    .map_err(|e| miette::miette!("status body: {e}"))?
            );
        }
    }
    Ok(())
}
