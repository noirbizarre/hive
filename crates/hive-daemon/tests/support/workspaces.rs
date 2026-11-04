//! Session admission tests observe both the Git checkout and the downstream ACP scope.

use super::*;
use hive_daemon::config::ConfigSource;
use serde_json::{Value, json};
use std::{path::Path, path::PathBuf, process::Command};
use tokio::io::{AsyncBufReadExt, BufReader};

struct Fixture {
    directory: tempfile::TempDir,
    seed: PathBuf,
    source: ConfigSource,
    version: AgentVersion,
}

fn git(directory: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        // User hooks and signing settings must not make the local fixture interactive.
        .args(["-c", "core.hooksPath=", "-c", "commit.gpgsign=false"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}

impl Fixture {
    fn new(version: AgentVersion, reverse: bool) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let base = directory.path().canonicalize().unwrap();
        let seed = base.join("seed");
        std::fs::create_dir(&seed).unwrap();
        git(&seed, &["init", "--initial-branch=main"]);
        std::fs::write(seed.join("committed.txt"), "workspace seed\n").unwrap();
        git(&seed, &["add", "committed.txt"]);
        git(
            &seed,
            &[
                "-c",
                "user.name=Hive Test",
                "-c",
                "user.email=hive@example.test",
                "commit",
                "-m",
                "seed",
            ],
        );
        std::fs::write(
            base.join("projects.toml"),
            "[projects.demo]\nname = 'Demo'\nrepository_source = 'https://example.test/project'\ndefault_ref = 'main'\n",
        )
        .unwrap();
        // Only the host subprocess sees this rewrite; parallel tests retain their own Git environment.
        std::fs::write(
            base.join("gitconfig"),
            format!(
                "[url {:?}]\n insteadOf = https://example.test/project\n[protocol \"file\"]\n allow = always\n[core]\n hooksPath =\n",
                reqwest::Url::from_file_path(&seed).unwrap().as_str()
            ),
        ).unwrap();
        std::fs::write(
            base.join("policies.toml"),
            "[workspaces.task]\npolicy = 'worktree'\n[worktrunk]\nenabled = false\n",
        )
        .unwrap();
        let (version_name, args) = match version {
            AgentVersion::V1 => ("v1", "['v1']"),
            AgentVersion::V2 => ("v2", "[]"),
        };
        // Agents stopped by the broker cannot flush reliable coverage profiles, including in-process hosts.
        let coverage = if std::env::var_os("LLVM_PROFILE_FILE").is_some() {
            format!(
                "LLVM_PROFILE_FILE = {:?}\n",
                if cfg!(windows) { "NUL" } else { "/dev/null" }
            )
        } else {
            String::new()
        };
        std::fs::write(
            base.join("agents.toml"),
            format!(
                "[agents.coder]\ncommand = {:?}\nargs = {args}\nversions = ['{version_name}']\nproject = 'demo'\nworkspace = 'task'\n[agents.coder.env]\nHIVE_MOCK_STATE = {:?}\nHIVE_MOCK_STARTED = {:?}\nPRIVATE_TOKEN = 'must-not-leak'\n{coverage}",
                env!("CARGO_BIN_EXE_hive-test-agent"),
                base.join("agent.json").to_str().unwrap(),
                base.join("started").to_str().unwrap(),
            ),
        )
        .unwrap();
        let includes = if reverse {
            "['policies.toml', 'projects.toml', 'agents.toml']"
        } else {
            "['agents.toml', 'projects.toml', 'policies.toml']"
        };
        let root = base.join("hive.toml");
        std::fs::write(
            &root,
            format!("default_agent = 'coder'\ninclude = {includes}\n"),
        )
        .unwrap();
        Self {
            directory,
            seed,
            source: ConfigSource::new(Some(root)).unwrap(),
            version,
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.directory.path().join(name)
    }

    async fn start(&self) -> (SocketAddr, tokio::process::Child) {
        // Portable project URLs stay valid while the isolated host resolves them entirely offline.
        let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "workspaces::workspace_gateway_host_serves_only_when_requested_by_a_parent_process",
                "--nocapture",
            ])
            .env("HIVE_WORKSPACE_HOST", self.directory.path())
            .env("GIT_CONFIG_GLOBAL", self.path("gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            // The host is killed deliberately; do not leave partial raw profiles in coverage runs.
            .env(
                "LLVM_PROFILE_FILE",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            )
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let address = timeout(Duration::from_secs(15), async {
            while let Some(line) = lines.next_line().await.unwrap() {
                if let Some(address) = line.strip_prefix("HIVE_WORKSPACE_READY ") {
                    return address.parse().unwrap();
                }
            }
            panic!("workspace host exited before publishing its listener");
        })
        .await
        .unwrap();
        (address, child)
    }

    fn assert_downstream_cwd(&self, cwd: &Path) {
        let state: Value =
            serde_json::from_slice(&std::fs::read(self.path("agent.json")).unwrap()).unwrap();
        let key = match self.version {
            AgentVersion::V1 => "v1_sessions",
            AgentVersion::V2 => "sessions",
        };
        assert!(
            state[key]
                .as_object()
                .unwrap()
                .values()
                .any(|session| session[0].as_str() == cwd.to_str())
        );
    }
}

#[tokio::test]
async fn workspace_gateway_host_serves_only_when_requested_by_a_parent_process() {
    let Some(base) = std::env::var_os("HIVE_WORKSPACE_HOST") else {
        return;
    };
    let base = PathBuf::from(base);
    let source = ConfigSource::new(Some(base.join("hive.toml"))).unwrap();
    let broker =
        hive_daemon::Broker::persistent(source.load().unwrap(), base.join("state/catalog.sqlite3"))
            .unwrap()
            .with_config_source(source);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    // Readiness is a pipe barrier rather than a timing-dependent startup delay.
    println!("HIVE_WORKSPACE_READY {}", listener.local_addr().unwrap());
    axum::serve(listener, hive_daemon::router(broker))
        .await
        .unwrap();
}

async fn serve(broker: &hive_daemon::Broker) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = hive_daemon::router(broker.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (address, server)
}

fn selector(value: Value) -> v2::Meta {
    let mut meta = v2::Meta::new();
    meta.insert("hive".into(), json!({"workspace":value}));
    meta
}

async fn inventory(address: SocketAddr) -> Value {
    reqwest::get(format!("http://{address}/v1/workspaces"))
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap()
}

#[tokio::test]
async fn workspace_worktrees_reach_both_acp_adapters_from_composed_config_in_either_order() {
    for version in [AgentVersion::V1, AgentVersion::V2] {
        for reverse in [false, true] {
            let fixture = Fixture::new(version, reverse);
            let (address, mut host) = fixture.start().await;
            timeout(Duration::from_secs(30), Client.v2().connect_with(
                HttpClient::new(format!("ws://{address}")).unwrap(), async |cx| {
                    cx.send_request(v2::InitializeRequest::new(ProtocolVersion::V2, v2::Implementation::new("workspace-test", "1"))).block_task().await?;
                    let mut paths = Vec::new();
                    let mut ids = Vec::new();
                    let mut branches = Vec::new();
                    for explicit in [true, false] {
                        // An unusable placeholder catches accidental fallback instead of provisioning.
                        let mut request = v2::NewSessionRequest::new(fixture.path("placeholder"));
                        if explicit {
                            let mut meta = selector(json!({"kind":"new","project":"demo","policy":"task","baseRef":"main","branch":"requested-task"}));
                            meta.get_mut("hive").unwrap()["agent"] = json!("coder");
                            request = request.meta(meta);
                        }
                        let opened = cx.send_request(request).block_task().await?;
                        let context = &opened.meta.as_ref().unwrap()["hive"]["workspace"];
                        assert_eq!(context.as_object().unwrap().len(), 3);
                        assert_eq!(context["project"], "demo");
                        let cwd = PathBuf::from(context["effectiveCwd"].as_str().unwrap());
                        assert_ne!(cwd, fixture.seed);
                        assert!(cwd.join(".git").is_file(), "expected a linked worktree");
                        assert_eq!(std::fs::read_to_string(cwd.join("committed.txt")).unwrap(), "workspace seed\n");
                        assert_eq!(git(&cwd, &["rev-parse", "HEAD"]), git(&fixture.seed, &["rev-parse", "HEAD"]));
                        let branch = git(&cwd, &["branch", "--show-current"]);
                        if explicit { assert_eq!(branch, "requested-task"); }
                        fixture.assert_downstream_cwd(&cwd);
                        let catalog = summaries(address).await;
                        let session = catalog["sessions"].as_array().unwrap().iter().find(|entry| entry["sessionId"] == opened.session_id.to_string()).unwrap();
                        assert_eq!(session["cwd"], context["effectiveCwd"]);
                        assert_eq!(&session["workspace"], context);
                        assert!(!serde_json::to_string(context).unwrap().contains("must-not-leak"));
                        let resources = inventory(address).await;
                        assert!(resources["workspaces"].as_array().unwrap().iter().any(|entry| entry["workspace"]["id"] == context["workspace"]));
                        ids.push(context["workspace"].clone());
                        paths.push(cwd.clone());
                        branches.push(branch);
                        let resumed = cx.send_request(v2::ResumeSessionRequest::new(opened.session_id.clone(), cwd)).block_task().await?;
                        assert_eq!(&resumed.meta.as_ref().unwrap()["hive"]["workspace"], context);
                        cx.send_request(v2::CloseSessionRequest::new(opened.session_id).meta(resumed.meta)).block_task().await?;
                    }
                    assert_ne!(ids[0], ids[1]);
                    assert_ne!(paths[0], paths[1]);
                    assert_ne!(branches[0], branches[1]);
                    let attached = cx.send_request(v2::NewSessionRequest::new(fixture.path("placeholder")).meta(selector(json!({"kind":"attach","id":ids[0]})))).block_task().await?;
                    let context = &attached.meta.as_ref().unwrap()["hive"]["workspace"];
                    assert_eq!(context["workspace"], ids[0]);
                    assert_eq!(context["effectiveCwd"].as_str(), paths[0].to_str());
                    fixture.assert_downstream_cwd(&paths[0]);
                    cx.send_request(v2::CloseSessionRequest::new(attached.session_id).meta(attached.meta)).block_task().await?;
                    assert_eq!(inventory(address).await["workspaces"].as_array().unwrap().len(), 2);
                    assert!(paths.iter().all(|path| path.is_dir()));
                    Ok(())
                },
            )).await.unwrap().unwrap();
            host.kill().await.unwrap();
            host.wait().await.unwrap();
        }
    }
}

#[tokio::test]
async fn workspace_legacy_cwd_and_explicit_existing_launches_preserve_the_downstream_directory() {
    for version in [AgentVersion::V1, AgentVersion::V2] {
        for explicit in [false, true] {
            let fixture = Fixture::new(version, false);
            let mut config = fixture.source.load().unwrap();
            if !explicit {
                config.agents.get_mut("coder").unwrap().workspace = None;
            }
            let broker =
                hive_daemon::Broker::persistent(config, fixture.path("state/catalog.sqlite3"))
                    .unwrap();
            let (address, server) = serve(&broker).await;
            timeout(
                Duration::from_secs(15),
                Client.v2().connect_with(
                    HttpClient::new(format!("ws://{address}")).unwrap(),
                    async |cx| {
                        cx.send_request(v2::InitializeRequest::new(
                            ProtocolVersion::V2,
                            v2::Implementation::new("legacy-workspace-test", "1"),
                        ))
                        .block_task()
                        .await?;
                        let mut request = v2::NewSessionRequest::new(fixture.seed.clone());
                        if explicit {
                            request = v2::NewSessionRequest::new(fixture.path("placeholder")).meta(
                                selector(json!({"kind":"existing","directory":fixture.seed})),
                            );
                        }
                        let opened = cx.send_request(request).block_task().await?;
                        fixture.assert_downstream_cwd(&fixture.seed);
                        assert!(opened.meta.as_ref().unwrap()["hive"]["workspace"].is_null());
                        assert_eq!(
                            summaries(address).await["sessions"][0]["cwd"].as_str(),
                            fixture.seed.to_str()
                        );
                        assert!(
                            inventory(address).await["workspaces"]
                                .as_array()
                                .unwrap()
                                .is_empty()
                        );
                        cx.send_request(
                            v2::CloseSessionRequest::new(opened.session_id).meta(opened.meta),
                        )
                        .block_task()
                        .await?;
                        Ok(())
                    },
                ),
            )
            .await
            .unwrap()
            .unwrap();
            broker.shutdown().await;
            server.abort();
        }
    }
}

#[tokio::test]
async fn workspace_selector_diagnostics_reject_private_and_mistyped_values_before_startup() {
    let fixture = Fixture::new(AgentVersion::V2, false);
    let broker = hive_daemon::Broker::persistent(
        fixture.source.load().unwrap(),
        fixture.path("state/catalog.sqlite3"),
    )
    .unwrap();
    // Selector validation needs no Git rewrite, so keep this boundary visible to coverage.
    let (address, server) = serve(&broker).await;
    timeout(
        Duration::from_secs(15),
        Client.v2().connect_with(
            HttpClient::new(format!("ws://{address}")).unwrap(),
            async |cx| {
                cx.send_request(v2::InitializeRequest::new(
                    ProtocolVersion::V2,
                    v2::Implementation::new("workspace-selector-test", "1"),
                ))
                .block_task()
                .await?;
                for launch in [
                    json!({"kind":"must-not-leak"}),
                    json!({"kind":"existing","directory":42}),
                    json!({"kind":"new","unexpected":"must-not-leak"}),
                ] {
                    let error = cx
                        .send_request(
                            v2::NewSessionRequest::new(fixture.seed.clone()).meta(selector(launch)),
                        )
                        .block_task()
                        .await
                        .unwrap_err();
                    let error = serde_json::to_value(error).unwrap();
                    assert_eq!(error["code"], -32602);
                    assert_eq!(error["data"]["code"], "hive::workspace::selector");
                    assert!(!error["data"]["message"].as_str().unwrap().is_empty());
                    assert!(!error["data"]["help"].as_str().unwrap().is_empty());
                    assert!(!error.to_string().contains("must-not-leak"));
                }
                assert!(
                    summaries(address).await["sessions"]
                        .as_array()
                        .unwrap()
                        .is_empty()
                );
                assert!(
                    inventory(address).await["workspaces"]
                        .as_array()
                        .unwrap()
                        .is_empty()
                );
                assert!(!fixture.path("started").exists());
                assert!(!fixture.path("agent.json").exists());
                Ok(())
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    broker.shutdown().await;
    server.abort();
}

#[tokio::test]
async fn workspace_failures_are_structured_and_leave_no_agent_session_or_profile_binding() {
    let fixture = Fixture::new(AgentVersion::V2, false);
    let (address, mut host) = fixture.start().await;
    timeout(
        Duration::from_secs(30),
        Client.v2().connect_with(
            HttpClient::new(format!("ws://{address}")).unwrap(),
            async |cx| {
                cx.send_request(v2::InitializeRequest::new(
                    ProtocolVersion::V2,
                    v2::Implementation::new("workspace-failure-test", "1"),
                ))
                .block_task()
                .await?;
                for (launch, code, rpc_code) in [
                    (
                        json!({"kind":"new","project":"unknown"}),
                        "hive::workspace::unknown",
                        -32600,
                    ),
                    (
                        json!({"kind":"new","policy":"unknown"}),
                        "hive::workspace::request",
                        -32600,
                    ),
                    (
                        json!({"kind":"existing","directory":fixture.path("missing")}),
                        "hive::workspace::request",
                        -32600,
                    ),
                    (
                        json!({"kind":"new","baseRef":"nonexistent-base"}),
                        "hive::workspace::request",
                        -32600,
                    ),
                ] {
                    let error = cx
                        .send_request(
                            v2::NewSessionRequest::new(fixture.seed.clone()).meta(selector(launch)),
                        )
                        .block_task()
                        .await
                        .unwrap_err();
                    let error = serde_json::to_value(error).unwrap();
                    assert_eq!(error["code"], rpc_code);
                    assert_eq!(error["data"]["code"], code);
                    assert!(!error["data"]["message"].as_str().unwrap().is_empty());
                    assert!(!error["data"]["help"].as_str().unwrap().is_empty());
                    assert!(!error.to_string().contains("must-not-leak"));
                    assert!(
                        summaries(address).await["sessions"]
                            .as_array()
                            .unwrap()
                            .is_empty()
                    );
                    assert!(
                        !fixture.path("started").exists(),
                        "provisioning failure started an agent process"
                    );
                    assert!(!fixture.path("agent.json").exists());
                }
                // Changing profile kind would be forbidden if a failed admission retained its binding.
                std::fs::write(
                    fixture.path("agents.toml"),
                    "[agents.coder]\nkind = 'opencode-server'\nendpoint = 'http://127.0.0.1:1'\n",
                )
                .unwrap();
                reqwest::Client::new()
                    .post(format!("http://{address}/v1/config/reload"))
                    .send()
                    .await
                    .unwrap()
                    .error_for_status()
                    .unwrap();
                Ok(())
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    host.kill().await.unwrap();
    host.wait().await.unwrap();
    // A fresh broker exposes persisted/offline rows as well as live sessions.
    let (address, mut host) = fixture.start().await;
    assert!(
        summaries(address).await["sessions"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        !inventory(address).await["workspaces"]
            .as_array()
            .unwrap()
            .is_empty(),
        "failed provisioning intent must remain inspectable"
    );
    host.kill().await.unwrap();
    host.wait().await.unwrap();
}
