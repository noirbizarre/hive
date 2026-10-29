//! Registry commands use the daemon as the sole writer of durable project metadata.

use clap::{Args, Subcommand};
use reqwest::Method;
use serde_json::{Value, json};
use std::time::Duration;

#[derive(Args)]
pub struct Options {
    #[arg(long, env = "HIVE_URL", global = true)]
    url: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Persist metadata without cloning or preparing the project.
    Register {
        /// Allocate an ID automatically when omitted.
        #[arg(long)]
        id: Option<String>,
        #[command(flatten)]
        definition: Definition,
    },
    /// List configured and managed project definitions.
    List,
    /// Inspect one project definition.
    Show { id: String },
    /// Replace all metadata of a managed project, preserving its ID.
    Update {
        id: String,
        #[command(flatten)]
        definition: Definition,
    },
    /// Remove a managed definition without deleting directories or workspaces.
    Remove { id: String },
    /// Inspect or manage an existing-directory preference for a specific node.
    Override {
        #[command(subcommand)]
        command: OverrideCommand,
    },
}

#[derive(Args)]
struct Definition {
    #[arg(long)]
    name: String,
    #[arg(long)]
    remote: Option<String>,
    #[arg(long)]
    default_ref: Option<String>,
    /// JSON array of argv arrays, stored without execution (for example [["mise","install"]]).
    #[arg(long, default_value = "[]")]
    preparation: String,
}

impl Definition {
    fn value(self) -> miette::Result<Value> {
        let preparation: Vec<Vec<String>> =
            serde_json::from_str(&self.preparation).map_err(|_| {
                miette::miette!("invalid preparation; provide a JSON array of argv arrays")
            })?;
        Ok(
            json!({"name":self.name,"repository_source":self.remote,"default_ref":self.default_ref,"preparation":preparation}),
        )
    }
}

#[derive(Subcommand)]
enum OverrideCommand {
    Show {
        id: String,
        node: String,
    },
    Set {
        id: String,
        node: String,
        /// Absolute path on the owning execution node.
        directory: String,
        #[arg(long, value_parser = ["posix", "windows"], default_value = "posix")]
        syntax: String,
    },
    Remove {
        id: String,
        node: String,
    },
}

// Validate route components locally so a typo cannot change which endpoint receives a mutation.
fn project_path(id: &str) -> miette::Result<String> {
    if hive_core::identity::ProjectId::try_from(id.to_owned()).is_err() {
        return Err(miette::miette!(
            "invalid project ID; copy an ID from hive project list"
        ));
    }
    Ok(format!("/v1/projects/{id}"))
}

pub async fn run(options: Options) -> miette::Result<()> {
    let (method, path, body) = match options.command {
        Command::List => (Method::GET, "/v1/projects".into(), None),
        Command::Show { id } => (Method::GET, project_path(&id)?, None),
        Command::Remove { id } => (Method::DELETE, project_path(&id)?, None),
        Command::Register { id, definition } => {
            if let Some(id) = &id {
                project_path(id)?;
            }
            let mut body = definition.value()?;
            body["id"] = json!(id);
            (Method::POST, "/v1/projects".into(), Some(body))
        }
        Command::Update { id, definition } => {
            (Method::PUT, project_path(&id)?, Some(definition.value()?))
        }
        Command::Override { command } => {
            let (method, id, node, body) = match command {
                OverrideCommand::Show { id, node } => (Method::GET, id, node, None),
                OverrideCommand::Remove { id, node } => (Method::DELETE, id, node, None),
                OverrideCommand::Set {
                    id,
                    node,
                    directory,
                    syntax,
                } => {
                    let body = json!({"directory":{"node":node,"syntax":syntax,"path":directory}});
                    (Method::PUT, id, node, Some(body))
                }
            };
            if hive_core::identity::NodeId::try_from(node.clone()).is_err() {
                return Err(miette::miette!(
                    "invalid node ID; use a stable registered node ID"
                ));
            }
            (
                method,
                format!("{}/nodes/{node}/override", project_path(&id)?),
                body,
            )
        }
    };
    let url = format!("{}{path}", super::daemon_url(options.url));
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|_| miette::miette!("cannot initialize project HTTP client"))?;
    let mut request = client.request(method, url);
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().await.map_err(|_| {
        miette::miette!("project registry unavailable; check hive daemon status and HIVE_URL")
    })?;
    let status = response.status();
    let body: Value = response
        .json()
        .await
        .map_err(|_| miette::miette!("cannot read project response; check daemon compatibility"))?;
    if !status.is_success() {
        return Err(miette::miette!(
            "{}: {}; {}",
            body["code"].as_str().unwrap_or("hive::project::http"),
            body["message"].as_str().unwrap_or("project request failed"),
            body["help"]
                .as_str()
                .unwrap_or("check hive daemon status and retry")
        ));
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&body).expect("JSON response is serializable")
    );
    Ok(())
}
