use std::{net::SocketAddr, path::PathBuf, time::Duration};

use clap::builder::styling::{AnsiColor, Styles};
use clap::{Args, CommandFactory, Parser, Subcommand};
use clap_complete::{Shell, generate};
use hive_daemon::config::Config;
use hive_tui::Options as TuiOptions;

mod doctor;
mod zellij_bridge;

const HELP_STYLES: Styles = Styles::styled()
    .header(AnsiColor::Cyan.on_default().bold())
    .usage(AnsiColor::Cyan.on_default().bold())
    .literal(AnsiColor::Blue.on_default().bold())
    .placeholder(AnsiColor::Yellow.on_default())
    .valid(AnsiColor::Green.on_default())
    .invalid(AnsiColor::Red.on_default())
    .error(AnsiColor::Red.on_default().bold());

#[derive(Parser)]
#[command(
    name = "hive",
    version,
    about = "Local ACPv2 gateway for coding agents",
    styles = HELP_STYLES
)]
struct Cli {
    #[command(flatten)]
    tui: TuiOptions,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Open the terminal client.
    Tui(TuiOptions),
    /// Run the local gateway in the foreground or query its status.
    Daemon(DaemonOptions),
    /// Open the daemon's browser client.
    Web {
        #[arg(long, env = "HIVE_URL")]
        url: Option<String>,
    },
    /// Diagnose configuration, agent compatibility and daemon state.
    Doctor(DoctorOptions),
    /// Generate shell completions on stdout.
    Completions { shell: Shell },
    /// Legacy alias for `hive daemon`.
    Serve(ServeOptions),
    /// Legacy alias for `hive daemon status`.
    Status(StatusOptions),
    /// Serve short HTTP requests for the Zellij ACP client.
    ZellijBridge {
        #[arg(long, default_value = "127.0.0.1:8766")]
        listen: SocketAddr,
        #[arg(long, env = "HIVE_URL")]
        url: Option<String>,
    },
}

#[derive(Args)]
struct DaemonOptions {
    #[command(flatten)]
    serve: ServeOptions,
    #[command(subcommand)]
    command: Option<DaemonCommand>,
}

#[derive(Subcommand)]
enum DaemonCommand {
    /// Query the running daemon.
    Status(StatusOptions),
}

#[derive(Args)]
struct ServeOptions {
    #[arg(long, default_value = "127.0.0.1:8765", env = "HIVE_LISTEN")]
    listen: SocketAddr,
    #[arg(long, env = "HIVE_CONFIG")]
    config: Option<PathBuf>,
}

#[derive(Args)]
struct DoctorOptions {
    /// Address the daemon would listen on (checked, never bound for good).
    #[arg(long, default_value = "127.0.0.1:8765", env = "HIVE_LISTEN")]
    listen: SocketAddr,
    #[arg(long, env = "HIVE_CONFIG")]
    config: Option<PathBuf>,
    /// Print the report as JSON on stdout, for scripts.
    #[arg(long)]
    json: bool,
    /// Agent profiles to check; every profile when omitted.
    agents: Vec<String>,
}

#[derive(Args)]
struct StatusOptions {
    #[arg(long, env = "HIVE_URL")]
    url: Option<String>,
}

fn daemon_url(url: Option<String>) -> String {
    url.unwrap_or_else(|| {
        std::env::var("HIVE_LISTEN")
            .map(|listen| format!("http://{listen}"))
            .unwrap_or_else(|_| "http://127.0.0.1:8765".into())
    })
    .trim_end_matches('/')
    .to_owned()
}

async fn serve(options: ServeOptions) -> miette::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let registry = Config::load(options.config).map_err(|error| miette::miette!("{error}"))?;
    hive_daemon::serve(options.listen, registry)
        .await
        .map_err(|error| miette::miette!("cannot serve Hive on {}: {error}", options.listen))
}

async fn status(options: StatusOptions) -> miette::Result<()> {
    let url = format!("{}/v1/status", daemon_url(options.url));
    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .map_err(|error| miette::miette!("HTTP client: {error}"))?
        .get(&url)
        .send()
        .await
        .map_err(|error| {
            miette::miette!("daemon unavailable at {url}: {error}; start `hive daemon`")
        })?
        .error_for_status()
        .map_err(|error| miette::miette!("daemon status failed: {error}"))?;
    println!(
        "{}",
        response
            .text()
            .await
            .map_err(|error| miette::miette!("status body: {error}"))?
    );
    Ok(())
}

async fn doctor(options: DoctorOptions) -> miette::Result<()> {
    let mut checks = hive_daemon::doctor::Options::new(options.listen);
    checks.config = options.config;
    checks.agents = options.agents;
    let report = hive_daemon::doctor::run(checks).await;
    // The report is the diagnostic; a miette error on top would print it twice.
    if !doctor::print(&report, options.json)? {
        std::process::exit(1);
    }
    Ok(())
}

fn web(url: Option<String>) -> miette::Result<()> {
    let url = web_url(url);
    open_web(&url, webbrowser::open)
}

fn open_web(url: &str, open: impl FnOnce(&str) -> std::io::Result<()>) -> miette::Result<()> {
    open(url).map_err(|error| {
        miette::miette!("cannot open browser: {error}; open {url} manually after `hive daemon`")
    })
}

fn web_url(url: Option<String>) -> String {
    format!("{}/ui", daemon_url(url))
}

#[tokio::main]
async fn main() -> miette::Result<()> {
    let Cli { tui, command } = Cli::parse();
    match command {
        None => hive_tui::run(tui)
            .await
            .map_err(|error| miette::miette!("cannot start Hive TUI: {error}")),
        Some(Command::Tui(options)) => hive_tui::run(options)
            .await
            .map_err(|error| miette::miette!("cannot start Hive TUI: {error}")),
        Some(Command::Daemon(DaemonOptions {
            command: Some(DaemonCommand::Status(options)),
            ..
        }))
        | Some(Command::Status(options)) => status(options).await,
        Some(Command::Daemon(DaemonOptions {
            serve: options,
            command: None,
        }))
        | Some(Command::Serve(options)) => serve(options).await,
        Some(Command::Web { url }) => web(url),
        Some(Command::Doctor(options)) => doctor(options).await,
        Some(Command::Completions { shell }) => {
            generate(shell, &mut Cli::command(), "hive", &mut std::io::stdout());
            Ok(())
        }
        Some(Command::ZellijBridge { listen, url }) => {
            zellij_bridge::serve(listen, daemon_url(url)).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::ColorChoice;
    use clap::error::ErrorKind;

    #[test]
    fn a_bare_command_and_explicit_tui_accept_the_same_client_options() {
        let bare =
            Cli::try_parse_from(["hive", "--agent", "example", "--transport", "http"]).unwrap();
        assert!(bare.command.is_none());
        assert_eq!(bare.tui.agent.as_deref(), Some("example"));
        assert_eq!(bare.tui.transport, "http");

        let explicit =
            Cli::try_parse_from(["hive", "tui", "--agent", "example", "--transport", "http"])
                .unwrap();
        let Some(Command::Tui(options)) = explicit.command else {
            panic!("tui subcommand not parsed");
        };
        assert_eq!(options.agent, bare.tui.agent);
        assert_eq!(options.transport, bare.tui.transport);
        assert_eq!(options.address, bare.tui.address);
    }

    #[test]
    fn daemon_commands_and_legacy_aliases_preserve_their_options() {
        let cli = Cli::try_parse_from([
            "hive",
            "daemon",
            "--listen",
            "127.0.0.1:3210",
            "--config",
            "custom.toml",
        ])
        .unwrap();
        let Some(Command::Daemon(options)) = cli.command else {
            panic!("daemon subcommand not parsed");
        };
        assert!(options.command.is_none());
        assert_eq!(options.serve.listen.port(), 3210);
        assert_eq!(options.serve.config, Some(PathBuf::from("custom.toml")));
        assert!(matches!(
            Cli::try_parse_from(["hive", "serve"]).unwrap().command,
            Some(Command::Serve(_))
        ));

        let cli = Cli::try_parse_from(["hive", "daemon", "status", "--url", "http://localhost:2"])
            .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Daemon(DaemonOptions {
                command: Some(DaemonCommand::Status(StatusOptions { url: Some(_), .. })),
                ..
            }))
        ));
        assert!(matches!(
            Cli::try_parse_from(["hive", "status"]).unwrap().command,
            Some(Command::Status(_))
        ));
    }

    #[test]
    fn help_and_version_exit_before_starting_a_client() {
        for arguments in [
            vec!["hive", "--help"],
            vec!["hive", "--version"],
            vec!["hive", "daemon", "--help"],
            vec!["hive", "tui", "--help"],
            vec!["hive", "web", "--help"],
        ] {
            let error = Cli::try_parse_from(arguments).err().unwrap();
            assert!(matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ));
        }
    }

    #[test]
    fn colored_help_reaches_subcommands_but_can_be_rendered_without_escapes() {
        let colored = Cli::command()
            .color(ColorChoice::Always)
            .try_get_matches_from(["hive", "daemon", "--help"])
            .unwrap_err()
            .render()
            .ansi()
            .to_string();
        assert!(colored.contains("\x1b["));
        assert!(colored.contains("--listen"));

        let plain = Cli::command()
            .color(ColorChoice::Never)
            .try_get_matches_from(["hive", "daemon", "--help"])
            .unwrap_err()
            .to_string();
        assert!(!plain.contains("\x1b["));
        assert!(plain.contains("--listen"));
    }

    #[test]
    fn doctor_accepts_a_json_flag_and_an_agent_filter() {
        let cli = Cli::try_parse_from([
            "hive",
            "doctor",
            "--json",
            "--listen",
            "127.0.0.1:1",
            "pi",
            "other",
        ])
        .unwrap();
        let Some(Command::Doctor(options)) = cli.command else {
            panic!("doctor subcommand not parsed");
        };
        assert!(options.json);
        assert_eq!(options.agents, ["pi", "other"]);
        assert_eq!(options.listen.port(), 1);
    }

    #[test]
    fn web_address_points_to_the_existing_daemon_route() {
        assert_eq!(
            web_url(Some("http://127.0.0.1:9000/".into())),
            "http://127.0.0.1:9000/ui"
        );
    }

    #[test]
    fn a_browser_launch_failure_provides_the_url_for_manual_opening() {
        let url = web_url(Some("http://127.0.0.1:9000".into()));
        let error = open_web(&url, |_| Err(std::io::Error::other("no browser installed")))
            .unwrap_err()
            .to_string();
        assert!(error.contains("no browser installed"));
        assert!(error.contains(&url));
        assert!(error.contains("hive daemon"));
    }

    #[test]
    fn generated_completions_include_the_unified_commands() {
        let mut output = Vec::new();
        generate(Shell::Bash, &mut Cli::command(), "hive", &mut output);
        let script = String::from_utf8(output).unwrap();
        for name in ["tui", "daemon", "web", "doctor", "completions"] {
            assert!(
                script.contains(name),
                "missing {name} in generated completions"
            );
        }
    }
}
