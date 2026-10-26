//! `hive doctor`: explains why Hive cannot reach or use the configured agents.
//!
//! The report is plain data so the CLI can render it as text or JSON. Nothing here sends a
//! prompt: stdio and endpoint agents only receive `initialize`, OpenCode servers only answer
//! health probes. Findings carry only labels Hive controls (see `compat`): endpoint URLs,
//! headers, environment values, arguments and agent output are never copied into a report.

use std::{
    ffi::OsStr,
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Duration,
};

use hive_protocol::AgentVersion;
use miette::Diagnostic;
use serde::Serialize;
use thiserror::Error;

use crate::{
    compat::{self, CapabilityReport, CompatError},
    config::{self, Config, Profile, ProfileKind},
    opencode::{self, error::OpencodeError},
    persistence, runtime_base,
};

/// Node.js major version `pi-acp` needs (see docs/compatibility.md).
const PI_NODE_MAJOR: u32 = 22;

/// A stuck `node --version` must not hang the whole report.
const TOOL_TIMEOUT: Duration = Duration::from_secs(5);

/// The daemon answers locally; waiting longer only delays the report.
const DAEMON_TIMEOUT: Duration = Duration::from_secs(3);

/// Findings produced by doctor itself. Codes are public identifiers: never rename one.
#[derive(Debug, Error, Diagnostic, PartialEq, Eq)]
pub enum DoctorError {
    #[error("no agent named `{id}` is configured")]
    #[diagnostic(
        code(hive::doctor::unknown_agent),
        help("pass one of the ids under [agents] in the config, or no id to check every profile")
    )]
    UnknownAgent { id: String },

    #[error("no agent profile is configured")]
    #[diagnostic(
        code(hive::doctor::no_agents),
        help("add an [agents.<id>] table to the config file; see the README for examples")
    )]
    NoAgents,

    #[error("{what} was not found")]
    #[diagnostic(
        code(hive::doctor::missing_prerequisite),
        help(
            "the `pi-acp` adapter needs Node.js 22+ and Pi on PATH: install them, then `npm install -g pi-acp`"
        )
    )]
    MissingPrerequisite { what: &'static str },

    #[error("the listen address is not a loopback address")]
    #[diagnostic(
        code(hive::doctor::listen_not_loopback),
        help("the daemon refuses remote binds until it has authentication; use 127.0.0.1 or ::1")
    )]
    ListenNotLoopback,

    #[error("the listen address is already in use by another program")]
    #[diagnostic(
        code(hive::doctor::port_in_use),
        help("stop the program using it, or start Hive with another `--listen` address")
    )]
    PortInUse,

    #[error("another Hive daemon holds the lock but does not answer on the listen address")]
    #[diagnostic(
        code(hive::doctor::daemon_locked),
        help(
            "it may listen on another address (try `--listen`), still be starting, or be stuck; stop it before starting a second one"
        )
    )]
    DaemonLocked,

    #[error("the Hive runtime directory or lock is not private: {path}")]
    #[diagnostic(
        code(hive::doctor::unsafe_runtime_dir),
        help(
            "the daemon refuses to start here; make the directory mode 0700 and the lock a regular file you own"
        )
    )]
    UnsafeRuntimeDir { path: String },

    #[error("Hive cannot locate its runtime directory")]
    #[diagnostic(
        code(hive::doctor::no_runtime_dir),
        help("set XDG_RUNTIME_DIR or HOME so Hive can locate its lock")
    )]
    NoRuntimeDir,

    #[error("Hive cannot locate its state directory")]
    #[diagnostic(
        code(hive::doctor::no_state_dir),
        help("set XDG_STATE_HOME or HOME so Hive can store its sessions")
    )]
    NoStateDir,

    #[error("the probe stopped unexpectedly")]
    #[diagnostic(
        code(hive::doctor::probe_panicked),
        help("re-run with RUST_BACKTRACE=1 and report a bug")
    )]
    ProbePanicked,

    #[error("the Hive state directory or database is not private: {path}")]
    #[diagnostic(
        code(hive::doctor::unsafe_state_dir),
        help(
            "the daemon refuses to start here; make the directory mode 0700 and the database a regular file (mode 0600) you own"
        )
    )]
    UnsafeStateDir { path: String },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Pass,
    Warn,
    Fail,
    Skip,
}

/// One line of the report. `code` and `help` are present for anything that is not a pass.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Finding {
    pub status: Status,
    pub code: Option<String>,
    pub message: String,
    pub help: Option<String>,
}

impl Finding {
    pub fn pass(message: impl Into<String>) -> Self {
        Self {
            status: Status::Pass,
            code: None,
            message: message.into(),
            help: None,
        }
    }

    pub fn from_diagnostic(status: Status, error: &(impl Diagnostic + ?Sized)) -> Self {
        Self {
            status,
            code: error.code().map(|code| code.to_string()),
            message: error.to_string(),
            help: error.help().map(|help| help.to_string()),
        }
    }
}

/// What an agent advertised at `initialize`, with agent-supplied strings made printable.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Capabilities {
    pub agent_name: String,
    pub agent_version: String,
    pub protocol: AgentVersion,
    pub load_session: bool,
    pub resume: bool,
    pub list: bool,
    pub image: bool,
    pub audio: bool,
    pub embedded_context: bool,
    pub auth_methods: usize,
}

impl From<&CapabilityReport> for Capabilities {
    fn from(report: &CapabilityReport) -> Self {
        Self {
            // Agent-supplied: the same filter as `CapabilityReport::summary`.
            agent_name: compat::sanitize(&report.agent_name),
            agent_version: compat::sanitize(&report.agent_version),
            protocol: report.protocol,
            load_session: report.load_session,
            resume: report.resume,
            list: report.list,
            image: report.image,
            audio: report.audio,
            embedded_context: report.embedded_context,
            auth_methods: report.auth_methods,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigReport {
    pub path: String,
    pub status: Status,
    pub findings: Vec<Finding>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileReport {
    pub id: String,
    pub kind: &'static str,
    pub transport: &'static str,
    pub versions: Vec<AgentVersion>,
    pub status: Status,
    pub capabilities: Option<Capabilities>,
    pub findings: Vec<Finding>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonReport {
    pub listen: String,
    pub status: Status,
    pub findings: Vec<Finding>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub ok: bool,
    pub config: ConfigReport,
    pub profiles: Vec<ProfileReport>,
    pub daemon: DaemonReport,
}

impl Report {
    /// Whether any check failed; warnings alone do not fail the run.
    pub fn failed(&self) -> bool {
        !self.ok
    }
}

pub struct Options {
    pub config: Option<PathBuf>,
    pub listen: SocketAddr,
    /// Profile ids to check; empty means every profile.
    pub agents: Vec<String>,
    /// Overrides `$XDG_RUNTIME_DIR` (the parent of `hive/daemon.lock`); tests use it to avoid
    /// mutating the process environment.
    pub runtime_dir: Option<PathBuf>,
    /// Overrides `$XDG_STATE_HOME` (the parent of `hive/sessions.sqlite3`).
    pub state_dir: Option<PathBuf>,
}

impl Options {
    pub fn new(listen: SocketAddr) -> Self {
        Self {
            config: None,
            listen,
            agents: Vec::new(),
            runtime_dir: None,
            state_dir: None,
        }
    }
}

fn status_of(findings: &[Finding]) -> Status {
    if findings.iter().any(|f| f.status == Status::Fail) {
        Status::Fail
    } else if findings.iter().any(|f| f.status == Status::Warn) {
        Status::Warn
    } else {
        Status::Pass
    }
}

/// Runs every check and returns the report; never fails, failures are findings.
pub async fn run(options: Options) -> Report {
    let path = options.config.clone().unwrap_or_else(config::default_path);
    let mut config_report = ConfigReport {
        path: path.display().to_string(),
        status: Status::Pass,
        findings: Vec::new(),
    };
    let mut selected = Vec::new();
    let mut skipped = Vec::new();
    match Config::read(options.config.clone()) {
        Err(error) => config_report
            .findings
            .push(Finding::from_diagnostic(Status::Fail, &error)),
        Ok(config) => {
            let problems = config.problems();
            for problem in &problems {
                config_report
                    .findings
                    .push(Finding::from_diagnostic(Status::Fail, &problem.error));
            }
            for id in &options.agents {
                if !config.agents.contains_key(id) {
                    config_report.findings.push(Finding::from_diagnostic(
                        Status::Fail,
                        &DoctorError::UnknownAgent { id: id.clone() },
                    ));
                }
            }
            if config.agents.is_empty() {
                config_report.findings.push(Finding::from_diagnostic(
                    Status::Warn,
                    &DoctorError::NoAgents,
                ));
            }
            if problems.is_empty() && !config.agents.is_empty() {
                config_report.findings.push(Finding::pass(format!(
                    "{} agent profile(s) configured",
                    config.agents.len()
                )));
            }
            for (id, profile) in &config.agents {
                if !options.agents.is_empty() && !options.agents.contains(id) {
                    continue;
                }
                // A profile the daemon would refuse is not worth launching.
                if problems.iter().any(|p| p.agent.as_deref() == Some(id)) {
                    skipped.push((id.clone(), profile.clone()));
                } else {
                    selected.push((id.clone(), profile.clone()));
                }
            }
        }
    }
    config_report.status = status_of(&config_report.findings);

    let (mut profiles, daemon) = tokio::join!(check_profiles(selected), check_daemon(&options));
    profiles.extend(skipped.into_iter().map(|(id, profile)| {
        let findings = vec![Finding {
            status: Status::Skip,
            code: None,
            message: "not probed: fix the configuration problems above first".into(),
            help: None,
        }];
        profile_report(&id, &profile, Status::Skip, None, findings)
    }));
    profiles.sort_by(|a, b| a.id.cmp(&b.id));

    let ok = ![config_report.status, daemon.status]
        .into_iter()
        .chain(profiles.iter().map(|p| p.status))
        .any(|status| status == Status::Fail);
    Report {
        ok,
        config: config_report,
        profiles,
        daemon,
    }
}

fn profile_report(
    id: &str,
    profile: &Profile,
    status: Status,
    capabilities: Option<Capabilities>,
    findings: Vec<Finding>,
) -> ProfileReport {
    ProfileReport {
        id: id.to_owned(),
        kind: profile.kind.as_str(),
        transport: profile.transport(),
        versions: profile.versions.clone(),
        status,
        capabilities,
        findings,
    }
}

/// Probes concurrently: each stdio probe can take up to its 30 s budget, and a handful of
/// slow profiles would otherwise multiply that.
async fn check_profiles(profiles: Vec<(String, Profile)>) -> Vec<ProfileReport> {
    let handles: Vec<_> = profiles
        .into_iter()
        .map(|(id, profile)| {
            let task = tokio::spawn({
                let (id, profile) = (id.clone(), profile.clone());
                async move { check_profile(&id, &profile).await }
            });
            (id, profile, task)
        })
        .collect();
    let mut reports = Vec::new();
    for (id, profile, task) in handles {
        reports.push(match task.await {
            Ok(report) => report,
            // A panicking probe must still yield a failing entry rather than vanish.
            Err(_) => profile_report(
                &id,
                &profile,
                Status::Fail,
                None,
                vec![Finding::from_diagnostic(
                    Status::Fail,
                    &DoctorError::ProbePanicked,
                )],
            ),
        });
    }
    reports
}

async fn check_profile(id: &str, profile: &Profile) -> ProfileReport {
    let mut findings = Vec::new();
    let mut capabilities = None;
    if profile.kind == ProfileKind::OpencodeServer {
        match opencode::check(profile).await {
            Ok(health) => findings.push(Finding::pass(format!(
                "OpenCode {} answered (API: {})",
                // Server-supplied: printable characters only.
                compat::sanitize(&health.version),
                match (health.v1, health.v2) {
                    (true, true) => "v1, v2",
                    (true, false) => "v1",
                    _ => "v2",
                }
            ))),
            Err(error) => findings.push(opencode_finding(error)),
        }
    } else {
        if let Some(command) = &profile.command {
            let path = std::env::var_os("PATH");
            for error in pi_prerequisites(command, &profile.args, path.as_deref()).await {
                findings.push(Finding::from_diagnostic(Status::Warn, &error));
            }
        }
        let result = if profile.command.is_some() {
            // Includes the launch checks (missing binary, bare Pi) before spawning anything.
            compat::probe_initialize(profile).await
        } else {
            compat::probe_endpoint(profile).await
        };
        match result {
            Ok(report) => {
                findings.push(Finding::pass(report.summary()));
                capabilities = Some(Capabilities::from(&report));
            }
            Err(error) => findings.push(Finding::from_diagnostic(
                if error == CompatError::NotProbeable {
                    Status::Skip
                } else {
                    Status::Fail
                },
                &error,
            )),
        }
    }
    let status = status_of(&findings);
    profile_report(id, profile, status, capabilities, findings)
}

fn opencode_finding(error: OpencodeError) -> Finding {
    // The reported version comes from the server, so it is made printable like agent names.
    let error = match error {
        OpencodeError::UnsupportedVersion { found, minimum } => OpencodeError::UnsupportedVersion {
            found: compat::sanitize(&found),
            minimum,
        },
        other => other,
    };
    Finding::from_diagnostic(Status::Fail, &error)
}

fn is_pi_adapter(command: &str, args: &[String]) -> bool {
    let name = Path::new(command)
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    name == "pi-acp"
        || (name == "npx"
            && args
                .iter()
                .any(|arg| arg == "pi-acp" || arg.starts_with("pi-acp@")))
}

fn find_on_path(name: &str, path: Option<&OsStr>) -> Option<PathBuf> {
    // Resolved here (rather than left to `Command`) so a test can supply its own search path.
    std::env::split_paths(path?)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// `pi-acp` is a Node program that spawns Pi: when either is missing the failure shows up
/// as a bare handshake error, so name the cause up front. Warnings only: the probe decides.
async fn pi_prerequisites(
    command: &str,
    args: &[String],
    path: Option<&OsStr>,
) -> Vec<DoctorError> {
    let mut missing = Vec::new();
    if !is_pi_adapter(command, args) {
        return missing;
    }
    match find_on_path("node", path) {
        None => missing.push(DoctorError::MissingPrerequisite {
            what: "Node.js 22 or newer (`node`)",
        }),
        Some(node) => {
            if node_major(&node)
                .await
                .is_some_and(|major| major < PI_NODE_MAJOR)
            {
                missing.push(DoctorError::MissingPrerequisite {
                    what: "Node.js 22 or newer (found an older `node`)",
                });
            }
        }
    }
    if find_on_path("pi", path).is_none() {
        missing.push(DoctorError::MissingPrerequisite {
            what: "the `pi` command",
        });
    }
    missing
}

async fn node_major(node: &Path) -> Option<u32> {
    let output = tokio::time::timeout(
        TOOL_TIMEOUT,
        tokio::process::Command::new(node)
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    parse_node_major(&String::from_utf8_lossy(&output.stdout))
}

fn parse_node_major(output: &str) -> Option<u32> {
    output
        .trim()
        .trim_start_matches('v')
        .split('.')
        .next()?
        .parse()
        .ok()
}

#[derive(Debug, PartialEq, Eq)]
enum Lock {
    /// No runtime directory or lock file: no daemon has ever run here.
    Absent,
    Free,
    Held,
}

/// Looks at the lock without taking it exclusively and without creating anything, so a
/// diagnosis can neither start a daemon's worth of state nor block a daemon that is starting.
fn inspect_lock(base: &Path) -> Result<Lock, DoctorError> {
    let dir = base.join("hive");
    let unsafe_dir = |path: &Path| DoctorError::UnsafeRuntimeDir {
        path: path.display().to_string(),
    };
    let dir_meta = match std::fs::symlink_metadata(&dir) {
        Ok(meta) => meta,
        Err(_) => return Ok(Lock::Absent),
    };
    if !is_private_dir(&dir_meta) {
        return Err(unsafe_dir(&dir));
    }
    let path = dir.join("daemon.lock");
    match std::fs::symlink_metadata(&path) {
        Ok(meta) if meta.file_type().is_symlink() => return Err(unsafe_dir(&path)),
        Ok(_) => {}
        Err(_) => return Ok(Lock::Absent),
    }
    let file = match std::fs::OpenOptions::new().read(true).open(&path) {
        Ok(file) => file,
        Err(_) => return Err(unsafe_dir(&path)),
    };
    match file.metadata() {
        Ok(meta) if is_private_file(&meta, &dir_meta) => {}
        _ => return Err(unsafe_dir(&path)),
    }
    // A shared request conflicts only with the daemon's exclusive lock. Fully qualified
    // because std's own `File::try_lock_shared` (Rust 1.89+) would shadow the trait method.
    match fs2::FileExt::try_lock_shared(&file) {
        Ok(()) => Ok(Lock::Free),
        Err(error) => {
            let busy = error.kind() == std::io::ErrorKind::WouldBlock
                || error.raw_os_error() == fs2::lock_contended_error().raw_os_error();
            if busy {
                Ok(Lock::Held)
            } else {
                Err(unsafe_dir(&path))
            }
        }
    }
}

fn is_private_dir(meta: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        meta.is_dir() && meta.mode() & 0o077 == 0
    }
    #[cfg(not(unix))]
    {
        meta.is_dir()
    }
}

fn is_private_file(meta: &std::fs::Metadata, dir: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        meta.is_file() && meta.nlink() == 1 && meta.mode() & 0o077 == 0 && meta.uid() == dir.uid()
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        meta.is_file()
    }
}

/// Mirrors what `Store::open` would refuse, from metadata alone: opening the store would
/// create and migrate it.
fn inspect_state(base: &Path) -> Result<bool, DoctorError> {
    let dir = base.join("hive");
    let unsafe_state = |path: &Path| DoctorError::UnsafeStateDir {
        path: path.display().to_string(),
    };
    let dir_meta = match std::fs::symlink_metadata(&dir) {
        Ok(meta) => meta,
        Err(_) => return Ok(false),
    };
    if !is_private_dir(&dir_meta) {
        return Err(unsafe_state(&dir));
    }
    let database = dir.join("sessions.sqlite3");
    match std::fs::symlink_metadata(&database) {
        Ok(meta) if is_private_file(&meta, &dir_meta) => Ok(true),
        Ok(_) => Err(unsafe_state(&database)),
        Err(_) => Ok(false),
    }
}

async fn check_daemon(options: &Options) -> DaemonReport {
    let mut findings = Vec::new();
    if options.listen.ip().is_loopback() {
        findings.push(Finding::pass(format!(
            "listen address {} is a loopback address",
            options.listen
        )));
    } else {
        findings.push(Finding::from_diagnostic(
            Status::Fail,
            &DoctorError::ListenNotLoopback,
        ));
    }

    let runtime = options.runtime_dir.clone().map_or_else(runtime_base, Ok);
    let lock = match runtime {
        Ok(base) => inspect_lock(&base),
        Err(_) => {
            findings.push(Finding::from_diagnostic(
                Status::Warn,
                &DoctorError::NoRuntimeDir,
            ));
            Ok(Lock::Absent)
        }
    };
    match lock {
        Err(error) => findings.push(Finding::from_diagnostic(Status::Fail, &error)),
        Ok(Lock::Held) => match running_daemon(options.listen).await {
            Some(summary) => findings.push(Finding::pass(summary)),
            None => findings.push(Finding::from_diagnostic(
                Status::Warn,
                &DoctorError::DaemonLocked,
            )),
        },
        Ok(state) => {
            findings.push(Finding::pass(if state == Lock::Absent {
                "no daemon has run yet (no lock file)"
            } else {
                "no other daemon holds the lock"
            }));
            // Only worth binding when nobody holds the lock: otherwise the port is the daemon's.
            match tokio::net::TcpListener::bind(options.listen).await {
                Ok(_) => findings.push(Finding::pass("the listen address is free")),
                Err(_) => findings.push(Finding::from_diagnostic(
                    Status::Fail,
                    &DoctorError::PortInUse,
                )),
            }
        }
    }

    let state = options
        .state_dir
        .clone()
        .map_or_else(|| persistence::state_base().map_err(|_| ()), Ok);
    match state {
        Ok(base) => match inspect_state(&base) {
            Ok(true) => findings.push(Finding::pass(
                "the state directory and session database are private",
            )),
            Ok(false) => findings.push(Finding::pass(
                "the state directory or database does not exist yet; Hive creates it on first start",
            )),
            Err(error) => findings.push(Finding::from_diagnostic(Status::Fail, &error)),
        },
        Err(()) => findings.push(Finding::from_diagnostic(
            Status::Warn,
            &DoctorError::NoStateDir,
        )),
    }

    DaemonReport {
        listen: options.listen.to_string(),
        status: status_of(&findings),
        findings,
    }
}

/// Asks the daemon on `listen` to identify itself. Transport errors are discarded: their
/// text embeds the URL.
async fn running_daemon(listen: SocketAddr) -> Option<String> {
    let body: serde_json::Value = reqwest::Client::builder()
        .timeout(DAEMON_TIMEOUT)
        .build()
        .ok()?
        .get(format!("http://{listen}/v1/status"))
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .json()
        .await
        .ok()?;
    let version = compat::sanitize(body.get("version")?.as_str()?);
    let sessions = body.get("sessions").and_then(serde_json::Value::as_u64);
    Some(format!(
        "Hive {version} is already running on {listen}{}",
        sessions.map_or(String::new(), |n| format!(" ({n} session(s))"))
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temporary() -> PathBuf {
        std::env::temp_dir().join(format!("hive-doctor-{}", uuid::Uuid::new_v4()))
    }

    #[cfg(unix)]
    fn private_dir(path: &Path) {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
            .unwrap();
    }

    #[test]
    fn doctor_diagnostic_codes_are_stable_public_identifiers() {
        let errors = [
            DoctorError::UnknownAgent { id: "x".into() },
            DoctorError::NoAgents,
            DoctorError::MissingPrerequisite { what: "x" },
            DoctorError::ListenNotLoopback,
            DoctorError::PortInUse,
            DoctorError::DaemonLocked,
            DoctorError::UnsafeRuntimeDir { path: "x".into() },
            DoctorError::NoRuntimeDir,
            DoctorError::NoStateDir,
            DoctorError::ProbePanicked,
            DoctorError::UnsafeStateDir { path: "x".into() },
        ];
        let codes: Vec<_> = errors
            .iter()
            .map(|e| e.code().unwrap().to_string())
            .collect();
        assert_eq!(
            codes,
            [
                "hive::doctor::unknown_agent",
                "hive::doctor::no_agents",
                "hive::doctor::missing_prerequisite",
                "hive::doctor::listen_not_loopback",
                "hive::doctor::port_in_use",
                "hive::doctor::daemon_locked",
                "hive::doctor::unsafe_runtime_dir",
                "hive::doctor::no_runtime_dir",
                "hive::doctor::no_state_dir",
                "hive::doctor::probe_panicked",
                "hive::doctor::unsafe_state_dir",
            ]
        );
        assert!(errors.iter().all(|e| e.help().is_some()));
    }

    #[test]
    fn node_versions_are_parsed_from_their_banner() {
        assert_eq!(parse_node_major("v22.11.0\n"), Some(22));
        assert_eq!(parse_node_major("v8.0.0"), Some(8));
        assert_eq!(parse_node_major("garbage"), None);
    }

    #[test]
    fn only_the_pi_adapter_commands_need_pi_prerequisites() {
        assert!(is_pi_adapter("pi-acp", &[]));
        assert!(is_pi_adapter("/opt/bin/pi-acp", &[]));
        assert!(is_pi_adapter("npx", &["-y".into(), "pi-acp".into()]));
        assert!(!is_pi_adapter("npx", &["-y".into(), "other".into()]));
        assert!(!is_pi_adapter("pi", &[]));
    }

    #[test]
    fn a_directory_that_never_held_a_daemon_has_no_lock() {
        assert_eq!(inspect_lock(&temporary()), Ok(Lock::Absent));
    }

    #[cfg(unix)]
    #[test]
    fn a_lock_held_by_another_handle_is_detected_without_being_taken() {
        let base = temporary();
        private_dir(&base.join("hive"));
        let path = base.join("hive/daemon.lock");
        let holder = {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .open(&path)
                .unwrap()
        };
        assert_eq!(inspect_lock(&base), Ok(Lock::Free));
        fs2::FileExt::try_lock_exclusive(&holder).unwrap();
        assert_eq!(inspect_lock(&base), Ok(Lock::Held));
        drop(holder);
        // The inspection must not leave the lock taken.
        assert_eq!(inspect_lock(&base), Ok(Lock::Free));
    }

    #[cfg(unix)]
    #[test]
    fn a_group_readable_runtime_directory_is_reported_as_unsafe() {
        use std::os::unix::fs::PermissionsExt;
        let base = temporary();
        private_dir(&base.join("hive"));
        std::fs::set_permissions(base.join("hive"), std::fs::Permissions::from_mode(0o750))
            .unwrap();
        assert!(matches!(
            inspect_lock(&base),
            Err(DoctorError::UnsafeRuntimeDir { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn an_unsafe_state_directory_is_reported_and_a_missing_one_is_not() {
        use std::os::unix::fs::PermissionsExt;
        let base = temporary();
        assert_eq!(inspect_state(&base), Ok(false));
        private_dir(&base.join("hive"));
        assert_eq!(inspect_state(&base), Ok(false));
        std::fs::set_permissions(base.join("hive"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        assert!(matches!(
            inspect_state(&base),
            Err(DoctorError::UnsafeStateDir { .. })
        ));
    }

    #[tokio::test]
    async fn a_pi_adapter_without_node_or_pi_gets_a_warning_for_each() {
        let empty = temporary();
        std::fs::create_dir_all(&empty).unwrap();
        let missing = pi_prerequisites("pi-acp", &[], Some(empty.as_os_str())).await;
        assert_eq!(missing.len(), 2, "{missing:?}");
        assert!(
            missing
                .iter()
                .all(|e| e.code().unwrap().to_string() == "hive::doctor::missing_prerequisite")
        );
    }

    #[tokio::test]
    async fn other_commands_are_never_checked_for_pi_prerequisites() {
        let empty = temporary();
        assert!(
            pi_prerequisites("some-agent", &[], Some(empty.as_os_str()))
                .await
                .is_empty()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_old_node_is_reported_and_a_new_one_is_not() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temporary();
        std::fs::create_dir_all(&dir).unwrap();
        let script = |name: &str, body: &str| {
            let path = dir.join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        script("pi", "exit 0");
        script("node", "echo v18.19.0");
        let old = pi_prerequisites("pi-acp", &[], Some(dir.as_os_str())).await;
        assert_eq!(old.len(), 1, "{old:?}");
        assert!(old[0].to_string().contains("older"));
        script("node", "echo v22.1.0");
        assert!(
            pi_prerequisites("pi-acp", &[], Some(dir.as_os_str()))
                .await
                .is_empty()
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_lock_that_is_a_symlink_or_group_readable_is_reported_as_unsafe() {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let base = temporary();
        private_dir(&base.join("hive"));
        let path = base.join("hive/daemon.lock");
        std::os::unix::fs::symlink("/dev/null", &path).unwrap();
        assert!(matches!(
            inspect_lock(&base),
            Err(DoctorError::UnsafeRuntimeDir { .. })
        ));
        std::fs::remove_file(&path).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o644)
            .open(&path)
            .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            inspect_lock(&base),
            Err(DoctorError::UnsafeRuntimeDir { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_group_readable_database_is_reported_as_unsafe() {
        use std::os::unix::fs::PermissionsExt;
        let base = temporary();
        private_dir(&base.join("hive"));
        let database = base.join("hive/sessions.sqlite3");
        std::fs::write(&database, b"").unwrap();
        std::fs::set_permissions(&database, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(inspect_state(&base), Ok(true));
        std::fs::set_permissions(&database, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            inspect_state(&base),
            Err(DoctorError::UnsafeStateDir { .. })
        ));
    }
}
