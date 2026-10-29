//! `hive_daemon::doctor::run` against the deterministic mock agent: no real agent, no model costs.

#[path = "support/fake_opencode.rs"]
mod fake_opencode;

use std::path::{Path, PathBuf};

use hive_daemon::doctor::{Options, Report, Status, run};

const MOCK: &str = env!("CARGO_BIN_EXE_hive-test-agent");

/// A throwaway directory holding the config and standing in for the XDG directories.
struct Sandbox(PathBuf);

impl Sandbox {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("hive-doctor-it-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn options(&self, config: &str) -> Options {
        let path = self.0.join("config.toml");
        std::fs::write(&path, config).unwrap();
        let mut options = Options::new("127.0.0.1:0".parse().unwrap());
        options.config = Some(path);
        options.runtime_dir = Some(self.0.join("run"));
        options.state_dir = Some(self.0.join("state"));
        options
    }
}

fn codes(findings: &[hive_daemon::doctor::Finding]) -> Vec<&str> {
    findings.iter().filter_map(|f| f.code.as_deref()).collect()
}

fn profile<'a>(report: &'a Report, id: &str) -> &'a hive_daemon::doctor::ProfileReport {
    report.profiles.iter().find(|p| p.id == id).unwrap()
}

#[tokio::test]
async fn included_agents_are_probed_and_composed_semantic_errors_are_collected() {
    let sandbox = Sandbox::new();
    let fragment = sandbox.0.join("agents.toml");
    std::fs::write(&fragment, format!("[agents.good]\ncommand = '{MOCK}'\n")).unwrap();
    let report = run(sandbox.options("include = ['agents.toml']\ndefault_agent = 'good'")).await;
    assert!(!report.failed(), "{report:#?}");
    assert_eq!(profile(&report, "good").status, Status::Pass);
    std::fs::write(
        &fragment,
        format!("[agents.good]\ncommand = '{MOCK}'\n[agents.empty]\n"),
    )
    .unwrap();
    let report = run(sandbox.options("include = ['agents.toml']\ndefault_agent = 'missing'")).await;
    assert_eq!(
        codes(&report.config.findings),
        ["hive::config::default_agent", "hive::config::profile"]
    );
    assert_eq!(profile(&report, "good").status, Status::Pass);
    assert_eq!(profile(&report, "empty").status, Status::Skip);
}

#[tokio::test]
async fn healthy_v1_and_v2_agents_are_reported_with_their_capabilities() {
    let report = run(Sandbox::new().options(&format!(
        "[agents.modern]\ncommand = '{MOCK}'\n[agents.legacy]\ncommand = '{MOCK}'\nargs = ['v1']\nversions = ['v1']\n"
    )))
    .await;
    assert!(!report.failed(), "{report:#?}");
    let modern = profile(&report, "modern").capabilities.as_ref().unwrap();
    let legacy = profile(&report, "legacy").capabilities.as_ref().unwrap();
    assert_eq!(serde_json::to_value(modern.protocol).unwrap(), "v2");
    assert_eq!(serde_json::to_value(legacy.protocol).unwrap(), "v1");
}

#[tokio::test]
async fn a_profile_declaring_the_wrong_protocol_fails_with_its_compat_code_and_help() {
    let report = run(Sandbox::new().options(&format!(
        "[agents.wrong]\ncommand = '{MOCK}'\nargs = ['v1']\nversions = ['v2']\n"
    )))
    .await;
    assert!(report.failed());
    let finding = &profile(&report, "wrong").findings[0];
    assert_eq!(
        finding.code.as_deref(),
        Some("hive::compat::protocol_mismatch")
    );
    assert!(finding.help.is_some());
}

#[tokio::test]
async fn a_bare_pi_command_is_told_to_use_the_adapter() {
    let report = run(Sandbox::new().options("[agents.pi]\ncommand = 'pi'\n")).await;
    let finding = &profile(&report, "pi").findings[0];
    assert_eq!(
        finding.code.as_deref(),
        Some("hive::compat::pi_without_adapter")
    );
    assert!(finding.help.as_ref().unwrap().contains("pi-acp"));
}

#[tokio::test]
async fn a_missing_binary_is_reported_as_binary_missing() {
    let report =
        run(Sandbox::new().options("[agents.gone]\ncommand = 'hive-definitely-not-installed'\n"))
            .await;
    assert_eq!(
        profile(&report, "gone").findings[0].code.as_deref(),
        Some("hive::compat::binary_missing")
    );
}

#[tokio::test]
async fn one_broken_profile_fails_the_run_without_hiding_the_healthy_one() {
    let report = run(Sandbox::new().options(&format!(
        "[agents.good]\ncommand = '{MOCK}'\n[agents.gone]\ncommand = 'hive-definitely-not-installed'\n"
    )))
    .await;
    assert!(report.failed());
    assert_eq!(profile(&report, "good").status, Status::Pass);
    assert_eq!(profile(&report, "gone").status, Status::Fail);
}

#[tokio::test]
async fn an_invalid_config_reports_every_problem_and_probes_nothing() {
    let report = run(Sandbox::new().options(&format!(
        "default_agent = 'missing'\n[agents.good]\ncommand = '{MOCK}'\n[agents.empty]\n"
    )))
    .await;
    assert!(report.failed());
    assert_eq!(
        codes(&report.config.findings),
        ["hive::config::default_agent", "hive::config::profile"]
    );
    assert_eq!(profile(&report, "empty").status, Status::Skip);
    // The profile without problems is still checked.
    assert_eq!(profile(&report, "good").status, Status::Pass);
}

#[tokio::test]
async fn an_unparsable_config_fails_without_quoting_its_content() {
    let report = run(Sandbox::new().options("[agents.a]\ncommand = 'secret-value\n")).await;
    assert_eq!(codes(&report.config.findings), ["hive::config::syntax"]);
    assert!(!format!("{report:?}").contains("secret-value"));
}

#[tokio::test]
async fn an_unknown_agent_filter_is_a_failure() {
    let sandbox = Sandbox::new();
    let mut options = sandbox.options(&format!("[agents.good]\ncommand = '{MOCK}'\n"));
    options.agents = vec!["nope".into()];
    let report = run(options).await;
    assert!(report.failed());
    assert_eq!(
        codes(&report.config.findings),
        ["hive::doctor::unknown_agent"]
    );
    assert!(report.profiles.is_empty());
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

#[cfg(unix)]
#[tokio::test]
async fn a_lock_held_by_a_daemon_that_does_not_answer_is_a_warning() {
    use std::os::unix::fs::OpenOptionsExt;
    let sandbox = Sandbox::new();
    private_dir(&sandbox.0.join("run/hive"));
    let holder = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(sandbox.0.join("run/hive/daemon.lock"))
        .unwrap();
    fs2::FileExt::try_lock_exclusive(&holder).unwrap();
    let report = run(sandbox.options("")).await;
    assert!(codes(&report.daemon.findings).contains(&"hive::doctor::daemon_locked"));
    // A warning must not turn into a failing exit status on its own.
    assert_eq!(report.daemon.status, Status::Warn);
}

#[cfg(unix)]
#[tokio::test]
async fn a_group_readable_state_directory_fails_the_daemon_check() {
    use std::os::unix::fs::PermissionsExt;
    let sandbox = Sandbox::new();
    private_dir(&sandbox.0.join("state/hive"));
    std::fs::set_permissions(
        sandbox.0.join("state/hive"),
        std::fs::Permissions::from_mode(0o750),
    )
    .unwrap();
    let report = run(sandbox.options("")).await;
    assert!(report.failed());
    assert!(codes(&report.daemon.findings).contains(&"hive::doctor::unsafe_state_dir"));
}

#[tokio::test]
async fn a_non_loopback_listen_address_fails() {
    let sandbox = Sandbox::new();
    let mut options = sandbox.options("");
    options.listen = "0.0.0.0:0".parse().unwrap();
    let report = run(options).await;
    assert!(codes(&report.daemon.findings).contains(&"hive::doctor::listen_not_loopback"));
}

#[tokio::test]
async fn an_unreachable_endpoint_profile_fails_without_leaking_its_url_or_headers() {
    // Reserve a port, then free it so nothing answers there.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let report = run(Sandbox::new().options(&format!(
        "[agents.remote]\nendpoint = 'http://127.0.0.1:{port}/secret-path'\nheaders = {{ 'X-Secret' = 'secret-token' }}\n\
         [agents.local]\ncommand = '{MOCK}'\nenv = {{ SENTINEL = 'secret-env' }}\n"
    )))
    .await;
    assert!(report.failed());
    assert_eq!(
        profile(&report, "remote").findings[0].code.as_deref(),
        Some("hive::compat::endpoint_unreachable")
    );
    let text = serde_json::to_string(&report).unwrap();
    for secret in ["secret-path", "secret-token", "secret-env", "X-Secret"] {
        assert!(!text.contains(secret), "{secret} leaked into the report");
    }
}

#[tokio::test]
async fn a_listen_address_held_by_another_program_fails() {
    let sandbox = Sandbox::new();
    let held = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut options = sandbox.options("");
    options.listen = held.local_addr().unwrap();
    let report = run(options).await;
    assert!(report.failed());
    assert!(codes(&report.daemon.findings).contains(&"hive::doctor::port_in_use"));
}

#[tokio::test]
async fn a_healthy_opencode_server_is_reported_with_its_version() {
    let (_fake, url) = fake_opencode::start(fake_opencode::Options::default()).await;
    let report = run(Sandbox::new().options(&format!(
        "[agents.oc]\nkind = 'opencode-server'\nendpoint = '{url}'\n"
    )))
    .await;
    assert!(!report.failed(), "{report:#?}");
    let finding = &profile(&report, "oc").findings[0];
    assert_eq!(finding.status, Status::Pass);
    assert!(finding.message.contains("1.19.3"), "{}", finding.message);
    // The server's address must stay out of the report.
    assert!(!serde_json::to_string(&report).unwrap().contains(&url));
}

#[tokio::test]
async fn an_unsupported_opencode_server_fails_with_its_opencode_code() {
    let (_fake, url) = fake_opencode::start(fake_opencode::Options {
        version: "1.2.0",
        ..Default::default()
    })
    .await;
    let report = run(Sandbox::new().options(&format!(
        "[agents.oc]\nkind = 'opencode-server'\nendpoint = '{url}'\n"
    )))
    .await;
    assert!(report.failed());
    assert_eq!(
        profile(&report, "oc").findings[0].code.as_deref(),
        Some("hive::opencode::unsupported_version")
    );
}

#[tokio::test]
async fn an_opencode_server_that_rejects_the_credentials_names_the_fix_not_the_secret() {
    let (_fake, url) = fake_opencode::start(fake_opencode::Options {
        password: Some("right-password".into()),
        ..Default::default()
    })
    .await;
    // The variable is only ever read by name; this one is deliberately unset in the test process.
    let report = run(Sandbox::new().options(&format!(
        "[agents.oc]\nkind = 'opencode-server'\nendpoint = '{url}'\npassword_env = 'HIVE_DOCTOR_TEST_UNSET_PASSWORD'\n"
    )))
    .await;
    let finding = &profile(&report, "oc").findings[0];
    assert_eq!(
        finding.code.as_deref(),
        Some("hive::opencode::missing_credentials")
    );
    assert!(
        !serde_json::to_string(&report)
            .unwrap()
            .contains("right-password")
    );
}

#[tokio::test]
async fn an_opencode_server_with_nothing_listening_is_unreachable() {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let report = run(Sandbox::new().options(&format!(
        "[agents.oc]\nkind = 'opencode-server'\nendpoint = 'http://127.0.0.1:{port}'\n"
    )))
    .await;
    assert_eq!(
        profile(&report, "oc").findings[0].code.as_deref(),
        Some("hive::opencode::unreachable")
    );
}
