//! Agent launch checks and an initialize-only compatibility probe.
//!
//! The probe never starts a session or a prompt turn, so running it costs no model tokens.
//! Diagnostics deliberately carry only labels Hive controls: agent output can contain paths,
//! tokens or prompts, so it is never echoed into an error.

use std::{
    env,
    path::{Path, PathBuf},
    time::Duration,
};

use agent_client_protocol::{
    Client, InitializeOutcome,
    schema::{ProtocolVersion, v1, v2},
};
use hive_protocol::AgentVersion;
use miette::Diagnostic;
use thiserror::Error;

use crate::{
    config::{Profile, ProfileKind},
    transport,
};

/// Long enough for a cold `npx -y pi-acp`, short enough to fail before a user gives up.
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// A host that is down should fail fast, well before the handshake budget is spent.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Why an agent cannot be used through Hive. Codes are public identifiers: never rename one.
#[derive(Debug, Error, Diagnostic, PartialEq, Eq)]
pub enum CompatError {
    #[error("agent executable `{command}` was not found")]
    #[diagnostic(
        code(hive::compat::binary_missing),
        help(
            "install it or point the profile's `command` at an installed binary; for Pi, install the `pi-acp` adapter with `npm install -g pi-acp` (needs Node 22+ and Pi on PATH)"
        )
    )]
    BinaryMissing { command: String },

    #[error("`{command}` is Pi itself, which does not speak ACP")]
    #[diagnostic(
        code(hive::compat::pi_without_adapter),
        help(
            "Pi has no native ACP mode (`--mode rpc` is Pi's own protocol); set the profile to `command = \"pi-acp\"` with `versions = [\"v1\"]`, or `command = \"npx\"` with `args = [\"-y\", \"pi-acp\"]`"
        )
    )]
    PiWithoutAdapter { command: String },

    #[error("agent negotiated ACP {negotiated} but the profile declares {declared}")]
    #[diagnostic(
        code(hive::compat::protocol_mismatch),
        help("set the profile's `versions` to [\"{negotiated}\"], or upgrade the agent")
    )]
    ProtocolMismatch {
        declared: &'static str,
        negotiated: String,
    },

    #[error("agent does not advertise {capability}")]
    #[diagnostic(
        code(hive::compat::missing_capability),
        help("use an agent version that advertises it, or avoid features that depend on it")
    )]
    MissingCapability { capability: &'static str },

    #[error("agent did not answer `initialize` within {seconds}s")]
    #[diagnostic(
        code(hive::compat::startup_timeout),
        help(
            "run the profile's command by hand to see why it stalls (interactive programs such as a bare TUI never answer ACP), or check that the endpoint serves ACP"
        )
    )]
    StartupTimeout { seconds: u64 },

    #[error("agent failed the `initialize` handshake")]
    #[diagnostic(
        code(hive::compat::initialize_failed),
        help(
            "run the profile's command by hand and check that it speaks ACP on stdio and exits cleanly (its stderr usually names the cause), or check the endpoint, its headers and its declared `versions`"
        )
    )]
    InitializeFailed,

    #[error("profile has no agent command to probe")]
    #[diagnostic(
        code(hive::compat::not_probeable),
        help(
            "the stdio probe needs `command` on the profile; endpoint profiles use the endpoint probe"
        )
    )]
    NotProbeable,

    #[error("the agent endpoint did not accept a connection ({reason})")]
    #[diagnostic(
        code(hive::compat::endpoint_unreachable),
        help(
            "check that the agent is running and that the profile's `endpoint` is correct; Hive never prints the endpoint or headers, so compare them with the config by hand"
        )
    )]
    EndpointUnreachable { reason: &'static str },
}

/// Effective capabilities advertised at `initialize`, including the v2 session baseline.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapabilityReport {
    pub agent_name: String,
    pub agent_version: String,
    pub protocol: AgentVersion,
    /// v1: `session/load`. Always `false` on v2, which has `session/resume` instead.
    pub load_session: bool,
    /// v1: `sessionCapabilities.resume`; v2: baseline session support.
    pub resume: bool,
    pub list: bool,
    pub image: bool,
    pub audio: bool,
    pub embedded_context: bool,
    pub auth_methods: usize,
}

impl CapabilityReport {
    /// One stable `key=value` line, safe to print: only booleans, counts and agent identity.
    pub fn summary(&self) -> String {
        format!(
            "agent={} version={} protocol={} load_session={} resume={} list={} image={} audio={} embedded_context={} auth_methods={}",
            sanitize(&self.agent_name),
            sanitize(&self.agent_version),
            wire_name(self.protocol),
            self.load_session,
            self.resume,
            self.list,
            self.image,
            self.audio,
            self.embedded_context,
            self.auth_methods,
        )
    }
}

fn wire_name(version: AgentVersion) -> &'static str {
    match version {
        AgentVersion::V1 => "v1",
        AgentVersion::V2 => "v2",
    }
}

/// Keeps an agent-supplied string printable on one line, so it cannot forge extra report fields.
pub(crate) fn sanitize(value: &str) -> String {
    value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || ".-_+".contains(*ch))
        .take(64)
        .collect()
}

/// Rejects launch settings that cannot work, before any process is spawned.
pub fn check_launch(command: &str, args: &[String]) -> Result<(), CompatError> {
    reject_pi_without_adapter(command, args)?;
    if !executable_exists(command, env::var_os("PATH").as_deref()) {
        return Err(CompatError::BinaryMissing {
            command: command.into(),
        });
    }
    Ok(())
}

/// Pure (no filesystem access) so it behaves the same whether or not Pi is installed.
fn reject_pi_without_adapter(command: &str, args: &[String]) -> Result<(), CompatError> {
    if is_pi(command)
        // Leave a way out for a future Pi that ships a real ACP flag.
        && !args.iter().any(|arg| arg.to_ascii_lowercase().contains("acp"))
    {
        return Err(CompatError::PiWithoutAdapter {
            command: command.into(),
        });
    }
    Ok(())
}

fn is_pi(command: &str) -> bool {
    // Compare the file name only: `/usr/bin/pi` and `pi` are the same program.
    Path::new(command)
        .file_stem()
        .is_some_and(|name| name == "pi")
}

pub(crate) fn executable_exists(command: &str, path: Option<&std::ffi::OsStr>) -> bool {
    // A path with a separator is used as given; `PATH` is only for bare names.
    if command.contains(std::path::MAIN_SEPARATOR) {
        return Path::new(command).is_file();
    }
    path.is_some_and(|path| {
        env::split_paths(path).any(|dir| {
            let candidate: PathBuf = dir.join(command);
            candidate.is_file()
        })
    })
}

/// Starts the profile's agent, runs `initialize` and reports what it advertised.
pub async fn probe_initialize(profile: &Profile) -> Result<CapabilityReport, CompatError> {
    let Some(command) = &profile.command else {
        return Err(CompatError::NotProbeable);
    };
    check_launch(command, &profile.args)?;
    let agent = transport(profile).map_err(|_| CompatError::InitializeFailed)?;
    let declared = profile.preferred_version();
    let probe = async {
        match declared {
            AgentVersion::V1 => probe_v1(agent).await,
            AgentVersion::V2 => probe_v2(agent).await,
        }
    };
    tokio::time::timeout(PROBE_TIMEOUT, probe)
        .await
        .map_err(|_| CompatError::StartupTimeout {
            seconds: PROBE_TIMEOUT.as_secs(),
        })?
}

/// Connects to an ACP endpoint profile, runs `initialize` and reports what it advertised.
///
/// Like [`probe_initialize`] it sends no session or prompt. Failures carry fixed labels only:
/// the transport errors embed the endpoint URL, which may hold credentials.
pub async fn probe_endpoint(profile: &Profile) -> Result<CapabilityReport, CompatError> {
    let endpoint = match (&profile.command, &profile.endpoint, profile.kind) {
        (None, Some(endpoint), ProfileKind::Acp) => endpoint,
        _ => return Err(CompatError::NotProbeable),
    };
    let url = reqwest::Url::parse(endpoint).map_err(|_| CompatError::EndpointUnreachable {
        reason: "invalid endpoint",
    })?;
    let (Some(host), Some(port)) = (url.host_str(), url.port_or_known_default()) else {
        return Err(CompatError::EndpointUnreachable {
            reason: "invalid endpoint",
        });
    };
    // A plain TCP connect first: it separates "nothing is listening" from "it does not speak ACP".
    let host = host.trim_start_matches('[').trim_end_matches(']');
    match tokio::time::timeout(
        CONNECT_TIMEOUT,
        tokio::net::TcpStream::connect((host, port)),
    )
    .await
    {
        Ok(Ok(_)) => {}
        Ok(Err(_)) => {
            return Err(CompatError::EndpointUnreachable {
                reason: "connection failed",
            });
        }
        Err(_) => {
            return Err(CompatError::EndpointUnreachable {
                reason: "timed out",
            });
        }
    }
    let agent = transport(profile).map_err(|_| CompatError::InitializeFailed)?;
    let probe = async {
        match profile.preferred_version() {
            AgentVersion::V1 => probe_v1(agent).await,
            AgentVersion::V2 => probe_v2(agent).await,
        }
    };
    tokio::time::timeout(PROBE_TIMEOUT, probe)
        .await
        .map_err(|_| CompatError::StartupTimeout {
            seconds: PROBE_TIMEOUT.as_secs(),
        })?
}

async fn probe_v1(agent: crate::Downstream) -> Result<CapabilityReport, CompatError> {
    Client
        .builder()
        .connect_with(agent, async move |cx| {
            let response = cx
                .send_request(v1::InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await;
            Ok(response)
        })
        .await
        .map_err(|_| CompatError::InitializeFailed)?
        .map_err(|_| CompatError::InitializeFailed)
        .and_then(report_v1)
}

async fn probe_v2(agent: crate::Downstream) -> Result<CapabilityReport, CompatError> {
    Client
        .v2()
        .connect_with(agent, async move |cx| {
            let response = cx
                .send_initialize(v2::InitializeRequest::new(
                    ProtocolVersion::V2,
                    v2::Implementation::new("hive", env!("CARGO_PKG_VERSION")),
                ))
                .block_task()
                .await;
            Ok(response)
        })
        .await
        .map_err(|_| CompatError::InitializeFailed)?
        .map_err(|_| CompatError::InitializeFailed)
        .and_then(|outcome| match outcome {
            InitializeOutcome::Initialized(response) => report_v2(response),
            // A valid downgrade is diagnostic evidence, never permission to ignore the profile.
            InitializeOutcome::LowerVersion(mismatch) => Err(CompatError::ProtocolMismatch {
                declared: "v2",
                negotiated: negotiated(&mismatch.selected),
            }),
            _ => Err(CompatError::InitializeFailed),
        })
}

fn report_v1(response: v1::InitializeResponse) -> Result<CapabilityReport, CompatError> {
    if response.protocol_version != ProtocolVersion::V1 {
        return Err(CompatError::ProtocolMismatch {
            declared: "v1",
            negotiated: negotiated(&response.protocol_version),
        });
    }
    let capabilities = response.agent_capabilities;
    let info = response.agent_info;
    Ok(CapabilityReport {
        agent_name: info.as_ref().map_or_else(String::new, |i| i.name.clone()),
        agent_version: info.map_or_else(String::new, |i| i.version),
        protocol: AgentVersion::V1,
        load_session: capabilities.load_session,
        resume: capabilities.session_capabilities.resume.is_some(),
        list: capabilities.session_capabilities.list.is_some(),
        image: capabilities.prompt_capabilities.image,
        audio: capabilities.prompt_capabilities.audio,
        embedded_context: capabilities.prompt_capabilities.embedded_context,
        auth_methods: response.auth_methods.len(),
    })
}

fn report_v2(response: v2::InitializeResponse) -> Result<CapabilityReport, CompatError> {
    if response.protocol_version != ProtocolVersion::V2 {
        return Err(CompatError::ProtocolMismatch {
            declared: "v2",
            negotiated: negotiated(&response.protocol_version),
        });
    }
    // Hive's v2 path needs `session` to create anything at all.
    let Some(session) = response.capabilities.session else {
        return Err(CompatError::MissingCapability {
            capability: "ACPv2 session support",
        });
    };
    let prompt = session.prompt;
    Ok(CapabilityReport {
        agent_name: response.info.name,
        agent_version: response.info.version,
        protocol: AgentVersion::V2,
        load_session: false,
        resume: true,
        list: true,
        image: prompt.as_ref().is_some_and(|p| p.image.is_some()),
        audio: prompt.as_ref().is_some_and(|p| p.audio.is_some()),
        embedded_context: prompt
            .as_ref()
            .is_some_and(|p| p.embedded_context.is_some()),
        auth_methods: response.auth_methods.len(),
    })
}

/// `ProtocolVersion` prints as a number on the wire; name it the way profiles do.
fn negotiated(version: &ProtocolVersion) -> String {
    if *version == ProtocolVersion::V1 {
        "v1".into()
    } else if *version == ProtocolVersion::V2 {
        "v2".into()
    } else {
        "an unknown version".into()
    }
}

#[cfg(test)]
mod tests {
    use miette::Diagnostic;

    use super::*;

    fn code(error: &CompatError) -> String {
        error
            .code()
            .expect("every diagnostic has a code")
            .to_string()
    }

    #[test]
    fn a_bare_pi_command_is_rejected_because_it_would_start_the_tui_instead_of_acp() {
        let error = check_launch("pi", &[]).unwrap_err();
        assert_eq!(code(&error), "hive::compat::pi_without_adapter");
    }

    #[test]
    fn pi_rpc_mode_is_rejected_because_it_is_not_acp() {
        let args = vec!["--mode".to_string(), "rpc".to_string()];
        let error = check_launch("/usr/bin/pi", &args).unwrap_err();
        assert_eq!(code(&error), "hive::compat::pi_without_adapter");
    }

    #[test]
    fn pi_with_an_acp_argument_is_left_for_the_handshake_to_judge() {
        let args = vec!["--acp".to_string()];
        assert_eq!(reject_pi_without_adapter("pi", &args), Ok(()));
    }

    #[test]
    fn the_adapter_command_is_never_mistaken_for_pi() {
        assert_eq!(reject_pi_without_adapter("pi-acp", &[]), Ok(()));
        assert_eq!(
            reject_pi_without_adapter("npx", &["-y".into(), "pi-acp".into()]),
            Ok(())
        );
    }

    #[test]
    fn a_missing_binary_is_reported_with_an_install_hint() {
        let error = check_launch("hive-definitely-not-installed", &[]).unwrap_err();
        assert_eq!(code(&error), "hive::compat::binary_missing");
        assert!(error.help().unwrap().to_string().contains("pi-acp"));
    }

    #[test]
    fn binaries_are_found_on_the_given_search_path_only() {
        let dir = std::env::temp_dir();
        let file = dir.join(format!("hive-compat-{}", std::process::id()));
        std::fs::write(&file, b"").unwrap();
        let name = file.file_name().unwrap().to_str().unwrap().to_owned();
        assert!(executable_exists(&name, Some(dir.as_os_str())));
        assert!(!executable_exists(
            &name,
            Some(std::ffi::OsStr::new("/nonexistent"))
        ));
        assert!(!executable_exists(&name, None));
        std::fs::remove_file(file).unwrap();
    }

    #[test]
    fn a_v2_response_to_a_v1_probe_is_a_protocol_mismatch() {
        let error = report_v1(v1::InitializeResponse::new(ProtocolVersion::V2)).unwrap_err();
        assert_eq!(code(&error), "hive::compat::protocol_mismatch");
        assert!(error.help().unwrap().to_string().contains("v2"));
    }

    #[test]
    fn a_v1_response_to_a_v2_probe_is_a_protocol_mismatch() {
        let response =
            v2::InitializeResponse::new(ProtocolVersion::V1, v2::Implementation::new("agent", "1"));
        let error = report_v2(response).unwrap_err();
        assert_eq!(code(&error), "hive::compat::protocol_mismatch");
    }

    #[test]
    fn a_v2_agent_without_session_support_is_missing_a_capability() {
        let response =
            v2::InitializeResponse::new(ProtocolVersion::V2, v2::Implementation::new("agent", "1"));
        let error = report_v2(response).unwrap_err();
        assert_eq!(code(&error), "hive::compat::missing_capability");
    }

    #[test]
    fn a_v1_response_is_summarised_without_leaking_unsafe_characters() {
        let mut response = v1::InitializeResponse::new(ProtocolVersion::V1);
        response.agent_info = Some(v1::Implementation::new("pi-acp\nfake=1", "0.0.34"));
        response.agent_capabilities.load_session = true;
        let report = report_v1(response).unwrap();
        assert!(report.load_session);
        let summary = report.summary();
        assert!(summary.contains("agent=pi-acpfake1 "));
        assert_eq!(summary.lines().count(), 1);
    }

    #[test]
    fn diagnostic_codes_are_stable_public_identifiers() {
        let codes = [
            CompatError::BinaryMissing {
                command: "x".into(),
            },
            CompatError::PiWithoutAdapter {
                command: "x".into(),
            },
            CompatError::ProtocolMismatch {
                declared: "v1",
                negotiated: "v2".into(),
            },
            CompatError::MissingCapability { capability: "x" },
            CompatError::StartupTimeout { seconds: 1 },
            CompatError::InitializeFailed,
            CompatError::NotProbeable,
            CompatError::EndpointUnreachable { reason: "x" },
        ]
        .map(|error| code(&error));
        assert_eq!(
            codes,
            [
                "hive::compat::binary_missing",
                "hive::compat::pi_without_adapter",
                "hive::compat::protocol_mismatch",
                "hive::compat::missing_capability",
                "hive::compat::startup_timeout",
                "hive::compat::initialize_failed",
                "hive::compat::not_probeable",
                "hive::compat::endpoint_unreachable",
            ]
        );
    }
}
