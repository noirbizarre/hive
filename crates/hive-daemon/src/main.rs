use std::net::SocketAddr;

use clap::{Parser, Subcommand};
use hive_daemon::config::Config;
use std::path::PathBuf;

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
        #[arg(long, env = "HIVE_CONFIG")]
        config: Option<PathBuf>,
    },
    Status {
        #[arg(long, env = "HIVE_URL")]
        url: Option<String>,
    },
}

#[tokio::main]
async fn main() -> miette::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    match Cli::parse().command {
        Command::Serve { listen, config } => {
            let registry = Config::load(config).map_err(|error| miette::miette!("{error}"))?;
            hive_daemon::serve(listen, registry)
                .await
                .map_err(|e| miette::miette!("cannot serve Hive on {listen}: {e}"))?;
        }
        Command::Status { url } => {
            let url = url.unwrap_or_else(|| {
                std::env::var("HIVE_LISTEN")
                    .map(|listen| format!("http://{listen}"))
                    .unwrap_or_else(|_| "http://127.0.0.1:8765".into())
            });
            let url = format!("{}/hive/v1/status", url.trim_end_matches('/'));
            let response = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(3))
                .build()
                .map_err(|e| miette::miette!("HTTP client: {e}"))?
                .get(&url)
                .send()
                .await
                .map_err(|e| {
                    miette::miette!("daemon unavailable at {url}: {e}; start `hive serve`")
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
