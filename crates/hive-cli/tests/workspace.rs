use serde_json::Value;
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Duration,
};

async fn daemon(root: &Path, address: SocketAddr) -> tokio::process::Child {
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_hive"))
        .args([
            "daemon",
            "--listen",
            &address.to_string(),
            "--config",
            "config.toml",
        ])
        .current_dir(root)
        .env("XDG_RUNTIME_DIR", root)
        .env("XDG_STATE_HOME", root)
        .env("GIT_CONFIG_GLOBAL", root.join("gitconfig"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
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
            assert!(child.try_wait().unwrap().is_none());
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    child
}

async fn command(address: SocketAddr, resource: &str, args: &[&str], success: bool) -> Value {
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_hive"))
        .arg(resource)
        .args(args)
        .env("HIVE_URL", format!("http://{address}"))
        .output()
        .await
        .unwrap();
    assert_eq!(
        output.status.success(),
        success,
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    if success {
        serde_json::from_slice(&output.stdout).unwrap()
    } else {
        assert!(String::from_utf8_lossy(&output.stderr).contains("unsafe_cleanup"));
        Value::Null
    }
}

fn git(root: &Path, directory: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .current_dir(directory)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", root.join("gitconfig"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn workspace_commands_provision_flat_checkouts_survive_restart_and_preserve_dirty_work_by_default()
 {
    let root = std::env::temp_dir().join(format!("hive-workspace-cli-{}", uuid::Uuid::new_v4()));
    let seed = root.join("seed");
    let remote = root.join("remote.git");
    std::fs::create_dir_all(&seed).unwrap();
    // Map a production-valid locator to a local remote without changing process-global Git configuration.
    std::fs::write(root.join("gitconfig"), format!("[user]\n name = Hive Tests\n email = hive@example.test\n[url \"{}\"]\n insteadOf = https://example.test/project\n[protocol \"file\"]\n allow = always\n", reqwest::Url::from_file_path(&remote).unwrap())).unwrap();
    git(&root, &seed, &["init", "-b", "main"]);
    std::fs::write(seed.join("content"), "original").unwrap();
    git(&root, &seed, &["add", "content"]);
    git(&root, &seed, &["commit", "-m", "initial"]);
    git(
        &root,
        &seed,
        &[
            "clone",
            "--bare",
            seed.to_str().unwrap(),
            remote.to_str().unwrap(),
        ],
    );
    std::fs::write(root.join("config.toml"), "[projects.demo]\nname = 'Demo'\nrepository_source = 'https://example.test/project'\n[workspaces.task]\npolicy = 'worktree'\n[worktrunk]\nenabled = false\n").unwrap();
    let reserve = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = reserve.local_addr().unwrap();
    drop(reserve);
    let mut child = daemon(&root, address).await;
    let first = command(
        address,
        "workspace",
        &["create", "--project", "demo", "--policy", "task"],
        true,
    )
    .await;
    let second = command(
        address,
        "workspace",
        &["create", "--project", "demo", "--policy", "task"],
        true,
    )
    .await;
    let first_id = first["workspace"]["id"].as_str().unwrap();
    let second_id = second["workspace"]["id"].as_str().unwrap();
    assert_ne!(first_id, second_id);
    let location = |entry: &Value| {
        PathBuf::from(
            entry["workspace"]["location"]["root"]["path"]
                .as_str()
                .unwrap(),
        )
    };
    let first_path = location(&first);
    let second_path = location(&second);
    let repositories = command(address, "repository", &["list"], true).await;
    let bare = PathBuf::from(
        repositories["repositories"][0]["root"]["path"]
            .as_str()
            .unwrap(),
    );
    assert_eq!(bare.file_name().unwrap(), ".git");
    assert_eq!(first_path.parent(), bare.parent());
    assert_eq!(second_path.parent(), bare.parent());
    let operation = first["workspace"]["operation"].as_str().unwrap();
    assert_eq!(
        command(address, "workspace", &["operation", operation], true).await["workspace"],
        first_id
    );
    std::fs::write(first_path.join("untracked"), "important").unwrap();
    command(address, "workspace", &["remove", first_id], false).await;
    child.kill().await.unwrap();
    child.wait().await.unwrap();
    let mut child = daemon(&root, address).await;
    assert_eq!(
        command(address, "repository", &["list"], true).await["node"],
        repositories["node"]
    );
    assert_eq!(
        command(address, "workspace", &["show", first_id], true).await["workspace"]["id"],
        first_id
    );
    command(address, "workspace", &["remove", second_id], true).await;
    assert!(!second_path.exists());
    assert_eq!(
        std::fs::read_to_string(first_path.join("untracked")).unwrap(),
        "important"
    );
    command(
        address,
        "workspace",
        &["remove", first_id, "--discard-changes"],
        true,
    )
    .await;
    assert!(!first_path.exists());
    let branch = first["workspace"]["location"]["branch"].as_str().unwrap();
    git(
        &root,
        &bare,
        &["show-ref", "--verify", &format!("refs/heads/{branch}")],
    );
    child.kill().await.unwrap();
    child.wait().await.unwrap();
    std::fs::remove_dir_all(root).unwrap();
}
