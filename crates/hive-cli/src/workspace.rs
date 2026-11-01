//! Explicit resource operations remain daemon-owned and print durable identities for retry.
use clap::{Args, Subcommand};
use hive_core::identity::{OperationId, ProjectId, RepositoryId, WorkspaceId};
use serde_json::{Value, json};

#[derive(Args)]
pub struct Options {
    #[arg(long, env = "HIVE_URL", global = true)]
    url: Option<String>,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Reconcile an interrupted operation against its exact registered Git identity.
    Reconcile {
        id: String,
        /// Actual path discovered manually when an integration's response was lost.
        #[arg(long)]
        directory: Option<std::path::PathBuf>,
        /// Confirm that uncertain WorkTrunk hook effects have been reviewed and reconciled.
        #[arg(long)]
        hooks_reconciled: bool,
    },
    /// Inspect durable request intent, resolved base, partial path and session holds.
    Operation {
        id: String,
    },
    List,
    Show {
        id: String,
    },
    Inspect {
        id: String,
    },
    /// Create from a configured policy. Preserve --operation to retry the same intent.
    Create {
        #[arg(long)]
        project: String,
        #[arg(long)]
        policy: String,
        #[arg(long)]
        base_ref: Option<String>,
        #[arg(long)]
        branch: Option<String>,
        #[arg(long)]
        operation: Option<String>,
    },
    /// Remove an idle managed workspace, preserving its branch.
    Remove {
        id: String,
        /// Discard local data; never overrides ownership or active-use checks.
        #[arg(long)]
        discard_changes: bool,
        #[arg(long)]
        operation: Option<String>,
    },
}

#[derive(Args)]
pub struct RepositoryOptions {
    #[arg(long, env = "HIVE_URL", global = true)]
    url: Option<String>,
    #[command(subcommand)]
    command: RepositoryCommand,
}
#[derive(Subcommand)]
enum RepositoryCommand {
    List,
    Show { id: String },
    Fetch { id: String },
}

fn operation(value: Option<String>) -> miette::Result<OperationId> {
    value
        .map(OperationId::try_from)
        .transpose()
        .map_err(|_| {
            miette::miette!("invalid operation ID; use the ID printed by the original request")
        })
        .map(|id| id.unwrap_or_default())
}

fn path(id: String) -> miette::Result<String> {
    let id = WorkspaceId::try_from(id)
        .map_err(|_| miette::miette!("invalid workspace ID; use hive workspace list"))?;
    Ok(format!("/v1/workspaces/{id}"))
}

pub async fn run(options: Options) -> miette::Result<()> {
    let (method, route, body) = match options.command {
        Command::Reconcile {
            id,
            directory,
            hooks_reconciled,
        } => (
            reqwest::Method::POST,
            format!("{}/reconcile", path(id)?),
            Some(json!({"directory":directory,"hooks_reconciled":hooks_reconciled})),
        ),
        Command::Operation { id } => {
            let id =
                OperationId::try_from(id).map_err(|_| miette::miette!("invalid operation ID"))?;
            (reqwest::Method::GET, format!("/v1/operations/{id}"), None)
        }
        Command::List => (reqwest::Method::GET, "/v1/workspaces".into(), None),
        Command::Show { id } => (reqwest::Method::GET, path(id)?, None),
        Command::Inspect { id } => (reqwest::Method::GET, format!("{}/inspect", path(id)?), None),
        Command::Create {
            project,
            policy,
            base_ref,
            branch,
            operation: key,
        } => {
            let project = ProjectId::try_from(project)
                .map_err(|_| miette::miette!("invalid project ID; use hive project list"))?;
            let operation = operation(key)?;
            // Print before network I/O so a lost response still leaves a retry key.
            eprintln!("operation: {operation}");
            (
                reqwest::Method::POST,
                "/v1/workspaces".into(),
                Some(
                    json!({"project":project,"policy":policy,"operation":operation,"base_ref":base_ref,"branch":branch}),
                ),
            )
        }
        Command::Remove {
            id,
            discard_changes,
            operation: key,
        } => {
            let operation = operation(key)?;
            eprintln!("operation: {operation}");
            (
                reqwest::Method::POST,
                format!("{}/remove", path(id)?),
                Some(json!({"operation":operation,"discard_changes":discard_changes})),
            )
        }
    };
    request(options.url, method, route, body).await
}

pub async fn repository(options: RepositoryOptions) -> miette::Result<()> {
    let (method, route) = match options.command {
        RepositoryCommand::List => (reqwest::Method::GET, "/v1/repositories".into()),
        command => {
            let (id, fetch) = match command {
                RepositoryCommand::Show { id } => (id, false),
                RepositoryCommand::Fetch { id } => (id, true),
                RepositoryCommand::List => unreachable!(),
            };
            let id = RepositoryId::try_from(id)
                .map_err(|_| miette::miette!("invalid repository ID; use hive repository list"))?;
            (
                if fetch {
                    reqwest::Method::POST
                } else {
                    reqwest::Method::GET
                },
                format!("/v1/repositories/{id}{}", if fetch { "/fetch" } else { "" }),
            )
        }
    };
    request(options.url, method, route, None).await
}

async fn request(
    url: Option<String>,
    method: reqwest::Method,
    route: String,
    body: Option<Value>,
) -> miette::Result<()> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .map_err(|_| miette::miette!("cannot initialize resource HTTP client"))?;
    let mut request = client.request(method, format!("{}{route}", super::daemon_url(url)));
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().await.map_err(|_| miette::miette!("resource request unavailable; inspect the original operation before retrying after a lost response"))?;
    let success = response.status().is_success();
    let body: Value = response
        .json()
        .await
        .map_err(|_| miette::miette!("invalid resource response; check daemon compatibility"))?;
    if !success {
        return Err(miette::miette!(
            "{}: {}; {}",
            body["code"].as_str().unwrap_or("hive::workspace::http"),
            body["message"]
                .as_str()
                .unwrap_or("resource request failed"),
            body["help"]
                .as_str()
                .unwrap_or("inspect the resource before retrying")
        ));
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&body).expect("JSON serializes")
    );
    Ok(())
}
