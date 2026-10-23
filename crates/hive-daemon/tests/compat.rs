//! `probe_initialize` against the deterministic mock agent: no real agent, no model costs.

use std::collections::BTreeMap;

use hive_daemon::{
    compat::{CompatError, probe_initialize},
    config::Profile,
};
use hive_protocol::AgentVersion;
use miette::Diagnostic;

fn profile(command: &str, args: &[&str], version: AgentVersion) -> Profile {
    Profile {
        name: None,
        command: Some(command.into()),
        args: args.iter().map(ToString::to_string).collect(),
        env: BTreeMap::new(),
        endpoint: None,
        headers: BTreeMap::new(),
        versions: vec![version],
        pricing: Default::default(),
        kind: Default::default(),
        username: None,
        password_env: None,
    }
}

const MOCK: &str = env!("CARGO_BIN_EXE_hive-test-agent");

#[tokio::test]
async fn a_v2_agent_is_reported_with_its_negotiated_protocol() {
    let report = probe_initialize(&profile(MOCK, &[], AgentVersion::V2))
        .await
        .unwrap();
    assert_eq!(report.protocol, AgentVersion::V2);
    assert!(!report.agent_name.is_empty());
}

#[tokio::test]
async fn a_v1_agent_is_reported_with_its_negotiated_protocol() {
    let report = probe_initialize(&profile(MOCK, &["v1"], AgentVersion::V1))
        .await
        .unwrap();
    assert_eq!(report.protocol, AgentVersion::V1);
}

#[tokio::test]
async fn a_profile_declaring_the_wrong_protocol_gets_an_actionable_diagnostic() {
    // The mock speaks v1 here, but the profile claims v2.
    let error = probe_initialize(&profile(MOCK, &["v1"], AgentVersion::V2))
        .await
        .unwrap_err();
    // This mock rejects a v2 handshake outright; an agent that downgrades (as pi-acp does)
    // yields `protocol_mismatch` instead, which the unit tests cover.
    assert_eq!(
        error.code().unwrap().to_string(),
        "hive::compat::initialize_failed"
    );
    assert!(
        error.help().is_some(),
        "every diagnostic must say what to do"
    );
}

#[tokio::test]
async fn a_missing_executable_is_reported_before_anything_is_spawned() {
    let error = probe_initialize(&profile(
        "hive-definitely-not-installed",
        &[],
        AgentVersion::V2,
    ))
    .await
    .unwrap_err();
    assert!(matches!(error, CompatError::BinaryMissing { .. }));
}

#[tokio::test]
async fn a_network_profile_is_not_probeable() {
    let mut network = profile(MOCK, &[], AgentVersion::V2);
    network.command = None;
    network.endpoint = Some("http://127.0.0.1:1".into());
    assert_eq!(
        probe_initialize(&network).await.unwrap_err(),
        CompatError::NotProbeable
    );
}

#[tokio::test]
async fn an_endpoint_with_nothing_listening_is_unreachable_without_naming_it() {
    // Reserve a port, then free it so nothing answers there.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut network = profile(MOCK, &[], AgentVersion::V2);
    network.command = None;
    network.endpoint = Some(format!("http://127.0.0.1:{port}/secret-path"));
    let error = hive_daemon::compat::probe_endpoint(&network)
        .await
        .unwrap_err();
    assert!(matches!(error, CompatError::EndpointUnreachable { .. }));
    let text = format!(
        "{error} {error:?} {:?}",
        error.help().map(|h| h.to_string())
    );
    assert!(!text.contains("secret-path") && !text.contains(&port.to_string()));
}

#[tokio::test]
async fn a_stdio_profile_is_not_an_endpoint_probe_target() {
    assert_eq!(
        hive_daemon::compat::probe_endpoint(&profile(MOCK, &[], AgentVersion::V2))
            .await
            .unwrap_err(),
        CompatError::NotProbeable
    );
}
