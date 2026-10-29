use serde_json::Value;
use std::{path::Path, time::Duration};

async fn daemon(runtime: &Path, address: std::net::SocketAddr) -> tokio::process::Child {
    let mut daemon = tokio::process::Command::new(env!("CARGO_BIN_EXE_hive"))
        .args([
            "daemon",
            "--listen",
            &address.to_string(),
            "--config",
            "config.toml",
        ])
        .current_dir(runtime)
        .env("XDG_RUNTIME_DIR", runtime)
        .env("XDG_STATE_HOME", runtime)
        .env_remove("HIVE_DEFAULT_AGENT")
        .env_remove("HIVE_CONFIG")
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if reqwest::get(format!("http://{address}/v1/ready"))
                .await
                .is_ok()
            {
                break;
            }
            assert!(daemon.try_wait().unwrap().is_none());
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    daemon
}

async fn project(address: std::net::SocketAddr, args: &[&str]) -> Value {
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_hive"))
        .arg("project")
        .args(args)
        .env("HIVE_URL", format!("http://{address}"))
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[tokio::test]
async fn project_commands_preserve_ids_across_daemon_restart_and_never_delete_local_work() {
    let runtime = std::env::temp_dir().join(format!("hive-project-cli-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&runtime).unwrap();
    std::fs::write(runtime.join("config.toml"), "").unwrap();
    let reserve = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = reserve.local_addr().unwrap();
    drop(reserve);
    let mut child = daemon(&runtime, address).await;
    let created = project(
        address,
        &[
            "register",
            "--name",
            "CLI project",
            "--remote",
            "git@example.test:repo",
            "--default-ref",
            "main",
            "--preparation",
            "[[\"unused-tool\"]]",
        ],
    )
    .await;
    let id = created["id"].as_str().unwrap();
    let node = format!("node-{}", uuid::Uuid::new_v4());
    let sentinel = runtime.join("keep-local-work");
    std::fs::write(&sentinel, "uncommitted").unwrap();
    project(
        address,
        &[
            "override",
            "set",
            id,
            &node,
            runtime.to_str().unwrap(),
            "--syntax",
            if cfg!(windows) { "windows" } else { "posix" },
        ],
    )
    .await;
    project(address, &["update", id, "--name", "Renamed"]).await;
    child.kill().await.unwrap();
    child.wait().await.unwrap();
    let mut child = daemon(&runtime, address).await;
    let shown = project(address, &["show", id]).await;
    assert_eq!(shown["id"], id);
    assert_eq!(shown["name"], "Renamed");
    assert!(shown["repository_source"].is_null());
    assert_eq!(
        project(address, &["list"]).await["projects"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        project(address, &["override", "show", id, &node]).await["directory"]["path"],
        runtime.to_str().unwrap()
    );
    project(address, &["remove", id]).await;
    assert!(
        project(address, &["list"]).await["projects"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(std::fs::read_to_string(&sentinel).unwrap(), "uncommitted");
    project(address, &["override", "show", id, &node]).await;
    project(address, &["override", "remove", id, &node]).await;
    child.kill().await.unwrap();
    child.wait().await.unwrap();
    std::fs::remove_dir_all(runtime).unwrap();
}
