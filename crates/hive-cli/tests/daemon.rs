use std::time::Duration;

use tokio::time::timeout;

#[tokio::test]
async fn cli_reload_and_hangup_apply_the_original_file_and_invalid_edits_leave_the_daemon_running()
{
    let runtime = std::env::temp_dir().join(format!("hive-reload-cli-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&runtime).unwrap();
    let path = runtime.join("config.toml");
    std::fs::write(&path, "").unwrap();
    let reserve = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = reserve.local_addr().unwrap();
    drop(reserve);
    let mut daemon = tokio::process::Command::new(env!("CARGO_BIN_EXE_hive"))
        .args([
            "daemon",
            "--listen",
            &address.to_string(),
            "--config",
            "config.toml",
        ])
        .current_dir(&runtime)
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("XDG_STATE_HOME", &runtime)
        .env_remove("HIVE_DEFAULT_AGENT")
        .env_remove("HIVE_CONFIG")
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let pid = daemon.id().unwrap();
    let status_url = format!("http://{address}/v1/status");
    timeout(Duration::from_secs(5), async {
        loop {
            if reqwest::get(&status_url).await.is_ok() {
                break;
            }
            assert!(daemon.try_wait().unwrap().is_none());
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    let config = |id: &str| {
        format!("default_agent = '{id}'\n[agents.{id}]\ncommand = 'unused-test-agent'\n")
    };
    let wait_default = async |id: &str| {
        timeout(Duration::from_secs(5), async {
            loop {
                let status: serde_json::Value = reqwest::get(&status_url)
                    .await
                    .unwrap()
                    .json()
                    .await
                    .unwrap();
                if status["defaultAgent"] == id {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .unwrap();
    };
    std::fs::write(&path, config("cli")).unwrap();
    let result = tokio::process::Command::new(env!("CARGO_BIN_EXE_hive"))
        .args(["daemon", "reload", "--url", &format!("http://{address}")])
        // A caller-selected file must not replace the running daemon's source.
        .env("HIVE_CONFIG", "does-not-exist.toml")
        .output()
        .await
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    wait_default("cli").await;
    std::fs::write(&path, "default_agent = 'missing'").unwrap();
    let rejected = tokio::process::Command::new(env!("CARGO_BIN_EXE_hive"))
        .args(["daemon", "reload"])
        .env("HIVE_URL", format!("http://{address}"))
        .output()
        .await
        .unwrap();
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("hive::config::default_agent"));
    wait_default("cli").await;
    #[cfg(unix)]
    {
        // A HUP failure must keep the handler alive for later valid updates.
        for id in [None, Some("hup"), Some("again")] {
            if let Some(id) = id {
                std::fs::write(&path, config(id)).unwrap();
            }
            assert!(
                tokio::process::Command::new("kill")
                    .args(["-HUP", &pid.to_string()])
                    .status()
                    .await
                    .unwrap()
                    .success()
            );
            if let Some(id) = id {
                wait_default(id).await;
            } else {
                // The explicit reload is serialized with HUP and acknowledges this invalid file.
                let result = reqwest::Client::new()
                    .post(format!("http://{address}/v1/config/reload"))
                    .send()
                    .await
                    .unwrap();
                assert_eq!(result.status(), 422);
            }
        }
        assert_eq!(daemon.id(), Some(pid));
        assert!(
            tokio::process::Command::new("kill")
                .args(["-TERM", &pid.to_string()])
                .status()
                .await
                .unwrap()
                .success()
        );
        assert!(
            timeout(Duration::from_secs(5), daemon.wait())
                .await
                .unwrap()
                .unwrap()
                .success()
        );
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        daemon.kill().await.unwrap();
    }
    std::fs::remove_dir_all(runtime).unwrap();
}

#[test]
fn redirected_help_stays_readable_without_terminal_escape_codes() {
    for args in [vec!["--help"], vec!["daemon", "--help"]] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_hive"))
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(!output.stdout.contains(&b'\x1b'));
    }
}

#[tokio::test]
async fn a_second_daemon_cannot_start_on_another_port_for_the_same_user() {
    let runtime = std::env::temp_dir().join(format!("hive-instance-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&runtime).unwrap();
    let reserve = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = reserve.local_addr().unwrap();
    drop(reserve);
    let first = tokio::process::Command::new(env!("CARGO_BIN_EXE_hive"))
        .args(["daemon", "--listen", &address.to_string()])
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("XDG_STATE_HOME", &runtime)
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut first = first;
    let status_url = format!("http://{address}/v1/status");
    timeout(Duration::from_secs(5), async {
        loop {
            if reqwest::get(&status_url).await.is_ok() {
                break;
            }
            assert!(
                first.try_wait().unwrap().is_none(),
                "first daemon exited early"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    let second = timeout(
        Duration::from_secs(5),
        tokio::process::Command::new(env!("CARGO_BIN_EXE_hive"))
            .args(["daemon", "--listen", "127.0.0.1:0"])
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("XDG_STATE_HOME", &runtime)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(!second.status.success());
    assert!(
        String::from_utf8_lossy(&second.stderr).contains("already running"),
        "duplicate startup reported: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(
        reqwest::get(&status_url)
            .await
            .unwrap()
            .status()
            .is_success()
    );
    let status = tokio::process::Command::new(env!("CARGO_BIN_EXE_hive"))
        .args(["daemon", "status", "--url", &format!("http://{address}")])
        .output()
        .await
        .unwrap();
    assert!(status.status.success());
    assert!(String::from_utf8_lossy(&status.stdout).contains("\"sessions\":0"));
    let legacy = tokio::process::Command::new(env!("CARGO_BIN_EXE_hive"))
        .args(["status", "--url", &format!("http://{address}")])
        .output()
        .await
        .unwrap();
    assert!(legacy.status.success());
    // A status query without --url must still honor the daemon's listen override.
    let from_listen = tokio::process::Command::new(env!("CARGO_BIN_EXE_hive"))
        .args(["daemon", "status"])
        .env("HIVE_LISTEN", address.to_string())
        .env_remove("HIVE_URL")
        .output()
        .await
        .unwrap();
    assert!(from_listen.status.success());
    #[cfg(unix)]
    {
        let signal = tokio::process::Command::new("kill")
            .args(["-TERM", &first.id().unwrap().to_string()])
            .status()
            .await
            .unwrap();
        assert!(signal.success());
        assert!(
            timeout(Duration::from_secs(5), first.wait())
                .await
                .unwrap()
                .unwrap()
                .success()
        );
    }
    #[cfg(not(unix))]
    first.kill().await.unwrap();
    let third = tokio::process::Command::new(env!("CARGO_BIN_EXE_hive"))
        .args(["serve", "--listen", &address.to_string()])
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("XDG_STATE_HOME", &runtime)
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut third = third;
    timeout(Duration::from_secs(5), async {
        loop {
            if reqwest::get(&status_url).await.is_ok() {
                break;
            }
            assert!(
                third.try_wait().unwrap().is_none(),
                "daemon could not restart"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    third.kill().await.unwrap();
    std::fs::remove_dir_all(runtime).unwrap();
}
