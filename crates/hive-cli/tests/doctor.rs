//! `hive doctor` end to end: exit status, JSON shape and what the text output may contain.

use std::{path::PathBuf, process::Output, time::Duration};

use tokio::time::timeout;

struct Sandbox(PathBuf);

impl Sandbox {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("hive-doctor-cli-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn config(&self, text: &str) -> PathBuf {
        let path = self.0.join("config.toml");
        std::fs::write(&path, text).unwrap();
        path
    }

    /// A `hive` invocation isolated from the developer's real config, lock and state.
    fn hive(&self, args: &[&str]) -> std::process::Command {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_hive"));
        command
            .args(args)
            .env("XDG_RUNTIME_DIR", &self.0)
            .env("XDG_STATE_HOME", &self.0)
            .env_remove("HIVE_CONFIG")
            .env_remove("HIVE_LISTEN")
            .env_remove("HIVE_DEFAULT_AGENT");
        command
    }
}

fn run(command: &mut std::process::Command) -> Output {
    command.output().unwrap()
}

#[test]
fn a_broken_config_exits_non_zero_and_still_prints_valid_json() {
    let sandbox = Sandbox::new();
    let config = sandbox.config("default_agent = 'missing'\n");
    let output = run(&mut sandbox.hive(&[
        "doctor",
        "--json",
        "--listen",
        "127.0.0.1:0",
        "--config",
        config.to_str().unwrap(),
    ]));
    assert_eq!(output.status.code(), Some(1));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["ok"], false);
    assert_eq!(
        report["config"]["findings"][0]["code"],
        "hive::config::default_agent"
    );
}

#[test]
fn a_config_without_problems_exits_zero() {
    let sandbox = Sandbox::new();
    let config = sandbox.config("");
    let output = run(&mut sandbox.hive(&[
        "doctor",
        "--listen",
        "127.0.0.1:0",
        "--config",
        config.to_str().unwrap(),
    ]));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("All checks passed"));
}

#[test]
fn the_text_report_names_the_diagnostic_code_and_the_fix() {
    let sandbox = Sandbox::new();
    let config = sandbox.config(
        "[agents.pi]\ncommand = 'pi'\n[agents.gone]\ncommand = 'hive-definitely-not-installed'\n",
    );
    let output = run(&mut sandbox.hive(&[
        "doctor",
        "--listen",
        "127.0.0.1:0",
        "--config",
        config.to_str().unwrap(),
    ]));
    assert_eq!(output.status.code(), Some(1));
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("hive::compat::pi_without_adapter"), "{text}");
    assert!(text.contains("hive::compat::binary_missing"), "{text}");
    assert!(text.contains("pi-acp"), "{text}");
}

#[test]
fn an_endpoint_profile_never_leaks_its_url_or_headers_into_either_output() {
    let sandbox = Sandbox::new();
    // Reserve a port, then free it so nothing answers there.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let config = sandbox.config(&format!(
        "[agents.remote]\nendpoint = 'http://127.0.0.1:{port}/secret-path'\nheaders = {{ 'X-Secret' = 'secret-token' }}\n"
    ));
    for json in [false, true] {
        let mut args = vec!["doctor", "--listen", "127.0.0.1:0", "--config"];
        args.push(config.to_str().unwrap());
        if json {
            args.push("--json");
        }
        let output = run(&mut sandbox.hive(&args));
        assert_eq!(output.status.code(), Some(1));
        let all = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(all.contains("hive::compat::endpoint_unreachable"), "{all}");
        for secret in ["secret-path", "secret-token", "X-Secret"] {
            assert!(!all.contains(secret), "{secret} leaked");
        }
    }
}

#[tokio::test]
async fn a_running_daemon_is_recognised_instead_of_reported_as_a_conflict() {
    let sandbox = Sandbox::new();
    let config = sandbox.config("");
    let reserve = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = reserve.local_addr().unwrap().to_string();
    drop(reserve);
    let mut daemon = tokio::process::Command::from(sandbox.hive(&[
        "daemon",
        "--listen",
        &address,
        "--config",
        config.to_str().unwrap(),
    ]))
    .kill_on_drop(true)
    .spawn()
    .unwrap();
    let status_url = format!("http://{address}/hive/v1/status");
    timeout(Duration::from_secs(5), async {
        loop {
            if reqwest::get(&status_url).await.is_ok() {
                break;
            }
            assert!(daemon.try_wait().unwrap().is_none(), "daemon exited early");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    let output = tokio::process::Command::from(sandbox.hive(&[
        "doctor",
        "--listen",
        &address,
        "--config",
        config.to_str().unwrap(),
    ]))
    .output()
    .await
    .unwrap();
    daemon.kill().await.unwrap();
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{text}");
    assert!(text.contains("already running"), "{text}");
}
