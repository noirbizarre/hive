//! Session admission tests observe the native SCM checkout and downstream ACP scope.

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
                "[url {:?}]\n insteadOf = https://example.test/project\n[protocol \"file\"]\n allow = always\n[core]\n hooksPath =\n[user]\n name = Hive Test\n email = hive@example.test\n[commit]\n gpgsign = false\n",
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
        let coverage = format!("LLVM_PROFILE_FILE = {:?}\n", mock_agent::PROFILE_FILE);
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
            // Owner lifecycle tools must not inherit personal hook or approval configuration.
            .env("HOME", self.directory.path())
            .env("XDG_CONFIG_HOME", self.path("config"))
            // JJ's per-repository trust/config files must also stay inside this fixture.
            .env("JJ_CONFIG", self.path("jj.toml"))
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
        // Reconciliation can resume an agent that runs native lifecycle tools before readiness.
        // Keep this total fixture ceiling distinct from the agent-silence watchdog.
        let address = timeout(Duration::from_secs(120), async {
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
    // Match daemon startup: settle retained sessions before announcing owner readiness.
    broker.reconcile().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    // Readiness is a pipe barrier rather than a timing-dependent startup delay.
    println!("HIVE_WORKSPACE_READY {}", listener.local_addr().unwrap());
    axum::serve(listener, hive_daemon::router(broker))
        .await
        .unwrap();
}

fn assert_vcs_report(report: &Value, owner: &str, backend: &str) {
    for key in ["vcsStatus", "vcsDiff", "vcsCommit", "vcsLog"] {
        assert_eq!(report[key]["isError"], false, "{key}: {}", report[key]);
        assert_eq!(report[key]["structuredContent"]["owner"], owner);
        assert_eq!(
            report[key]["structuredContent"]["repositoryBackend"],
            backend
        );
    }
    assert_eq!(
        report["vcsStatus"]["structuredContent"]["state"]["dirty"],
        true
    );
    let result = &report["vcsCommit"]["structuredContent"];
    assert_eq!(result["outcome"], "recorded");
    assert!(
        result["state"]["files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|file| file["path"] == "untracked.txt" && file["untracked"] == true)
    );
    let recorded = result["recorded"]["identity"]["commitId"].as_str().unwrap();
    assert!(
        report["vcsLog"]["structuredContent"]["history"]
            .as_array()
            .unwrap()
            .iter()
            .any(|change| change["identity"]["commitId"] == recorded)
    );
}

fn assert_native_vcs_record(root: &Path, report: &Value, backend: &str) {
    let recorded = report["vcsCommit"]["structuredContent"]["recorded"]["identity"]["commitId"]
        .as_str()
        .unwrap();
    let files = if backend == "git" {
        assert_eq!(git(root, &["rev-parse", "HEAD"]), recorded);
        git(root, &["ls-tree", "--name-only", "-r", recorded])
    } else {
        let output = Command::new("jj")
            .current_dir(root)
            .args([
                "--ignore-working-copy",
                "--no-integrate-operation",
                "file",
                "list",
                "-r",
                recorded,
                "-T",
                "path ++ \"\\n\"",
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    };
    assert!(files.contains("committed.txt"));
    assert!(!files.contains("untracked.txt"));
}

#[tokio::test]
async fn remote_git_workspace_capability_calls_execute_on_the_owner_during_startup_and_recovery() {
    remote_workspace_capabilities("git", "git.worktree").await;
}

#[tokio::test]
async fn remote_jj_workspace_capability_calls_execute_on_the_owner_during_startup_and_recovery() {
    remote_workspace_capabilities("jj", "jj.workspace").await;
}

#[tokio::test]
async fn remote_worktrunk_capability_calls_execute_on_the_owner_during_startup_and_recovery() {
    remote_workspace_capabilities("git", "worktrunk").await;
}

async fn remote_workspace_capabilities(scm: &str, provisioner: &str) {
    use hive_protocol::{remote::Peer, transport::Credentials};
    if provisioner == "worktrunk"
        && !Command::new("wt")
            .args(["config", "state", "marker", "--help"])
            .output()
            .is_ok_and(|output| {
                output.status.success()
                    && String::from_utf8_lossy(&output.stdout).contains("--branch")
            })
    {
        eprintln!("skipping workspace capability WorkTrunk case: compatible wt unavailable");
        return;
    }
    let fixture = Fixture::new(AgentVersion::V2, false);
    std::fs::write(fixture.path("agents.toml"), format!(
              "[agents.coder]\ncommand = {:?}\n[agents.coder.env]\nHIVE_MCP_REPORT = {:?}\nHIVE_WORKSPACE_TOOLS = '1'\nHIVE_VCS_EXERCISE = '1'\nLLVM_PROFILE_FILE = {:?}\n",
            env!("CARGO_BIN_EXE_hive-test-mcp-agent"), fixture.path("report.json"),
            // The recovery scenario deliberately terminates processes before profile flushing.
            if cfg!(windows) { "NUL" } else { "/dev/null" })).unwrap();
    if scm == "jj" {
        std::fs::write(fixture.path("projects.toml"), "[projects.demo]\nname = 'Demo'\nscm = 'jj'\nrepository_source = 'https://example.test/project'\ndefault_revision = 'main@origin'\n").unwrap();
        std::fs::write(fixture.path("jj.toml"), "[user]\nname = 'Hive Test'\nemail = 'hive@example.test'\n[signing]\nbehavior = 'drop'\n").unwrap();
    }
    std::fs::write(
        fixture.path("policies.toml"),
        format!(
            "[workspaces.task]\npolicy = '{}'\n[worktrunk]\nenabled = {}\n",
            if scm == "jj" { "jj" } else { "worktree" },
            provisioner == "worktrunk"
        ),
    )
    .unwrap();
    let initiating = tempfile::tempdir().unwrap();
    let placeholder = initiating.path().join("absent-placeholder");
    let (address, mut host) = fixture.start().await;
    let retained = std::sync::Mutex::new(None);
    timeout(
        // This is a total test ceiling, not the agent-silence watchdog: setup runs native tools.
        Duration::from_secs(120),
        Client.v2().connect_with(
            Credentials::default()
                .peer(&format!("http://{address}/acp"))
                .unwrap(),
            async |cx| {
                let peer = Peer::initialize(
                    cx.clone(),
                    v2::InitializeRequest::new(
                        ProtocolVersion::V2,
                        v2::Implementation::new("workspace-tool-peer", "1"),
                    ),
                )
                .await?;
                let opened = peer
                    .start(
                        "coder",
                        v2::NewSessionRequest::new(placeholder.clone()).meta(selector(
                            json!({"kind":"new","project":"demo","policy":"task"}),
                        )),
                    )
                    .await?;
                let report: Value =
                    serde_json::from_slice(&std::fs::read(fixture.path("report.json")).unwrap())
                        .unwrap();
                let actual = &report["workspaceDirect"]["structuredContent"];
                let context = &opened.meta.as_ref().unwrap()["hive"]["workspace"];
                assert_eq!(actual["owner"], peer.node);
                assert_eq!(actual["workspace"], context["workspace"]);
                assert_eq!(actual["project"], "demo");
                assert_eq!(actual["repositoryBackend"], scm);
                assert_eq!(actual["provisioner"], provisioner);
                assert_eq!(report["lists"]["tools"].as_array().unwrap().len(), 11);
                assert_eq!(actual["tools"].as_array().unwrap().len(), 11);
                assert_eq!(report["currentRemove"]["isError"], true);
                let refusal: Value = serde_json::from_str(
                    report["currentRemove"]["content"][0]["text"]
                        .as_str()
                        .unwrap(),
                )
                .unwrap();
                assert_eq!(refusal["code"], "hive::workspace::unsafe_cleanup");
                assert!(
                    refusal["operation"]
                        .as_str()
                        .unwrap()
                        .starts_with("operation-")
                );
                assert!(matches!(
                    refusal["reason"].as_str(),
                    Some("in_use" | "unknown_use")
                ));
                for key in [
                    "workspaceCurrent",
                    "workspaceInfo",
                    "siblingCreate",
                    "siblingReplay",
                    "siblingInfo",
                    "siblingRemove",
                    "siblingOperation",
                ] {
                    assert_eq!(report[key]["isError"], false, "{key}: {}", report[key]);
                    assert_eq!(report[key]["structuredContent"]["owner"], actual["owner"]);
                    assert_eq!(
                        report[key]["structuredContent"]["project"],
                        actual["project"]
                    );
                }
                assert_eq!(
                    report["siblingCreate"]["structuredContent"]["workspace"],
                    report["siblingReplay"]["structuredContent"]["workspace"]
                );
                assert_eq!(
                    report["siblingOperation"]["structuredContent"]["state"],
                    "removed"
                );
                assert_vcs_report(&report, &peer.node, scm);
                assert_native_vcs_record(
                    Path::new(context["effectiveCwd"].as_str().unwrap()),
                    &report,
                    scm,
                );
                assert_eq!(
                    inventory(address).await["workspaces"][0]["selection"]["provisioner"],
                    actual["provisioner"]
                );
                assert!(!placeholder.exists());
                *retained.lock().unwrap() = Some((opened.session_id, context.clone()));
                Ok(())
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    host.kill().await.unwrap();
    host.wait().await.unwrap();
    let (id, context) = retained.into_inner().unwrap().unwrap();
    let (address, mut host) = fixture.start().await;
    timeout(
        Duration::from_secs(120),
        Client.v2().connect_with(
            Credentials::default()
                .peer(&format!("http://{address}/acp"))
                .unwrap(),
            async |cx| {
                Peer::initialize(
                    cx.clone(),
                    v2::InitializeRequest::new(
                        ProtocolVersion::V2,
                        v2::Implementation::new("workspace-tool-peer", "1"),
                    ),
                )
                .await?;
                let opened = cx
                    .send_request(v2::ResumeSessionRequest::new(
                        id.clone(),
                        PathBuf::from(context["effectiveCwd"].as_str().unwrap()),
                    ))
                    .block_task()
                    .await?;
                let report: Value =
                    serde_json::from_slice(&std::fs::read(fixture.path("report.json")).unwrap())
                        .unwrap();
                assert_eq!(report["phase"], "resume");
                assert_vcs_report(&report, context["owner"].as_str().unwrap(), scm);
                assert_native_vcs_record(
                    Path::new(context["effectiveCwd"].as_str().unwrap()),
                    &report,
                    scm,
                );
                assert_eq!(
                    report["workspace"]["structuredContent"]["owner"],
                    context["owner"]
                );
                assert_eq!(
                    report["workspace"]["structuredContent"]["workspace"],
                    context["workspace"]
                );
                cx.send_request(v2::CloseSessionRequest::new(id.clone()).meta(opened.meta))
                    .block_task()
                    .await?;
                Ok(())
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(!placeholder.exists());
    assert!(!initiating.path().join("repositories").exists());
    host.kill().await.unwrap();
    host.wait().await.unwrap();
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
async fn remote_project_transfer_provisions_only_on_the_selected_owner_and_distinguishes_failures()
{
    use hive_protocol::{remote::Peer, transport::Credentials};
    for (version, worktrunk) in [
        (AgentVersion::V1, false),
        (AgentVersion::V2, false),
        (AgentVersion::V2, true),
    ] {
        // Windows can resolve `wt` to Windows Terminal rather than WorkTrunk.
        // Successful spawning alone does not establish the optional marker capability.
        if worktrunk
            && !Command::new("wt")
                .args(["config", "state", "marker", "--help"])
                .output()
                .is_ok_and(|output| {
                    let help = String::from_utf8_lossy(&output.stdout);
                    output.status.success()
                        && help.contains("--branch")
                        && help.contains("--format")
                })
        {
            eprintln!("skipping remote WorkTrunk case: compatible wt unavailable");
            continue;
        }
        let fixture = Fixture::new(version, false);
        if worktrunk {
            std::fs::write(
                fixture.path("policies.toml"),
                "[workspaces.task]\npolicy = 'worktree'\n[worktrunk]\nenabled = true\n",
            )
            .unwrap();
        }
        let config = fixture.source.load().unwrap();
        let mut definition = config.projects.values().next().unwrap().clone();
        // Successful startup proves stored preparation metadata did not become executable input.
        definition.preparation = vec![vec!["hive-nonexistent-preparation-command".into()]];
        // The selected owner starts with neither project metadata nor a checkout.
        std::fs::write(fixture.path("projects.toml"), "").unwrap();
        let agents = std::fs::read_to_string(fixture.path("agents.toml")).unwrap();
        std::fs::write(
            fixture.path("agents.toml"),
            agents
                .replace("project = 'demo'\n", "")
                .replace("workspace = 'task'\n", ""),
        )
        .unwrap();
        let initiating = tempfile::tempdir().unwrap();
        let hub = hive_daemon::Broker::persistent(
            Config::default(),
            initiating.path().join("state/catalog.sqlite3"),
        )
        .unwrap();
        hub.register_project(hive_daemon::projects::Registration {
            id: Some(hive_core::identity::ProjectId::try_from("transferred".to_owned()).unwrap()),
            definition,
        })
        .await
        .unwrap();
        let before = hub.projects().unwrap();
        let definition = before[0].definition.clone();
        let retained = std::sync::Mutex::new(None);
        let (address, mut host) = fixture.start().await;
        timeout(Duration::from_secs(30), Client.v2().connect_with(
            Credentials::default().peer(&format!("http://{address}/acp")).unwrap(), async |cx| {
                let peer = Peer::initialize(cx.clone(), v2::InitializeRequest::new(ProtocolVersion::V2, v2::Implementation::new("provisioning-peer", "1"))).await?;
                let invalid = v2::NewSessionRequest::new(initiating.path().join("absent-placeholder"))
                    .meta(selector(json!({"kind":"new","project":"invalid","definition":definition,"policy":"task","revision":"main"})));
                assert_eq!(peer.start("coder", invalid).await.unwrap_err().data.unwrap()["code"], "hive::remote::invalid_workspace");
                let projects: Value = reqwest::get(format!("http://{address}/v1/projects")).await.unwrap().json().await.unwrap();
                assert!(projects["projects"].as_array().unwrap().is_empty());
                assert!(!fixture.path("started").exists());
                let launch = |definition: Value| v2::NewSessionRequest::new(initiating.path().join("absent-placeholder"))
                    .meta(selector(json!({"kind":"new", "project":"transferred", "definition":definition, "policy":"task"})));
                let opened = peer.start("coder", launch(serde_json::to_value(&definition).unwrap())).await;
                assert!(opened.is_ok(), "remote startup failed: {opened:?}; owner resources: {}", inventory(address).await);
                let opened = opened?;
                let meta = &opened.meta.as_ref().unwrap()["hive"];
                let context = &meta["workspace"];
                assert_eq!(context["owner"], peer.node);
                assert_eq!(meta["session"], json!({"owner":peer.node,"id":opened.session_id}));
                let cwd = PathBuf::from(context["effectiveCwd"].as_str().unwrap());
                // Windows canonicalization adds a verbatim-path prefix; compare physical paths on both sides.
                assert!(cwd.canonicalize().unwrap().starts_with(fixture.path("state").canonicalize().unwrap()));
                assert!(cwd.join("committed.txt").is_file());
                assert!(!initiating.path().join("absent-placeholder").exists());
                fixture.assert_downstream_cwd(&cwd);
                if worktrunk {
                    let resources = inventory(address).await;
                    assert_eq!(resources["workspaces"][0]["selection"]["provisioner"], "worktrunk");
                    cx.send_request(v2::PromptRequest::new(opened.session_id.clone(), vec!["hello".into()]).meta(controlled(&opened.meta, "remote-marker"))).block_task().await?;
                    timeout(Duration::from_secs(10), async {
                        loop {
                            let output = Command::new("git").arg("-C").arg(&cwd).args(["config", "--get-regexp", "worktrunk.state.*.marker"]).output().unwrap();
                            if String::from_utf8_lossy(&output.stdout).contains("Hive") { break; }
                            tokio::time::sleep(Duration::from_millis(50)).await;
                        }
                    }).await.unwrap();
                }
                let resumed = cx.send_request(v2::ResumeSessionRequest::new(opened.session_id.clone(), cwd)).block_task().await?;
                assert_eq!(resumed.meta.as_ref().unwrap()["hive"]["session"], meta["session"]);
                cx.send_request(v2::CloseSessionRequest::new(opened.session_id).meta(resumed.meta)).block_task().await?;

                // The owner can subsequently resolve the transferred project by identity alone.
                let request = v2::NewSessionRequest::new(initiating.path().join("absent-placeholder"))
                    .meta(selector(json!({"kind":"new","project":"transferred","policy":"task"})));
                let known = peer.start("coder", request).await?;
                assert_eq!(known.meta.as_ref().unwrap()["hive"]["workspace"]["project"], "transferred");
                *retained.lock().unwrap() = Some((known.session_id, known.meta.unwrap()["hive"].clone()));

                let mut conflict = serde_json::to_value(&definition).unwrap();
                conflict["name"] = json!("Conflicting project");
                let error = peer.start("coder", launch(conflict)).await.unwrap_err();
                assert_eq!(error.data.unwrap()["code"], "hive::remote::project_conflict");

                let foreign = hive_core::identity::NodeId::new();
                let request = v2::NewSessionRequest::new(initiating.path().join("absent-placeholder"))
                    .meta(selector(json!({"kind":"attach","owner":foreign,"id":context["workspace"]})));
                assert_eq!(peer.start("coder", request).await.unwrap_err().data.unwrap()["code"], "hive::remote::invalid_workspace");

                // An unreachable source is mapped only inside the owner subprocess; no network is used.
                let mut missing = serde_json::to_value(&definition).unwrap();
                missing["repository_source"] = json!("https://example.test/project/missing");
                let request = v2::NewSessionRequest::new(initiating.path().join("absent-placeholder"))
                    .meta(selector(json!({"kind":"new","project":"unavailable","definition":missing,"policy":"task"})));
                let error = peer.start("coder", request).await.unwrap_err();
                assert_eq!(error.data.unwrap()["code"], "hive::remote::provisioning_failed");
                assert_eq!(inventory(address).await["workspaces"].as_array().unwrap().len(), 3);
                Ok(())
            },
        )).await.unwrap().unwrap();
        assert_eq!(hub.projects().unwrap().len(), before.len());
        assert!(!initiating.path().join("repositories").exists());
        assert!(!initiating.path().join("state/worktrunk-config").exists());
        hub.shutdown().await;
        host.kill().await.unwrap();
        host.wait().await.unwrap();
        let (id, metadata) = retained.into_inner().unwrap().unwrap();
        let (address, mut restarted) = fixture.start().await;
        timeout(
            Duration::from_secs(30),
            Client.v2().connect_with(
                Credentials::default()
                    .peer(&format!("http://{address}/acp"))
                    .unwrap(),
                async |cx| {
                    let peer = Peer::initialize(
                        cx.clone(),
                        v2::InitializeRequest::new(
                            ProtocolVersion::V2,
                            v2::Implementation::new("restarted-peer", "1"),
                        ),
                    )
                    .await?;
                    assert_eq!(metadata["session"]["owner"], peer.node);
                    let cwd =
                        PathBuf::from(metadata["workspace"]["effectiveCwd"].as_str().unwrap());
                    let resumed = cx
                        .send_request(v2::ResumeSessionRequest::new(id.clone(), cwd))
                        .block_task()
                        .await?;
                    let actual = &resumed.meta.as_ref().unwrap()["hive"];
                    assert_eq!(actual["session"], metadata["session"]);
                    assert_eq!(actual["workspace"], metadata["workspace"]);
                    cx.send_request(v2::CloseSessionRequest::new(id.clone()).meta(resumed.meta))
                        .block_task()
                        .await?;
                    Ok(())
                },
            ),
        )
        .await
        .unwrap()
        .unwrap();
        restarted.kill().await.unwrap();
        restarted.wait().await.unwrap();
    }
}

#[tokio::test]
async fn named_profiles_provision_inline_and_referenced_policies_across_transports_and_adapters() {
    use hive_protocol::remote::{ListParams, Peer};
    for version in [AgentVersion::V1, AgentVersion::V2] {
        let fixture = Fixture::new(version, false);
        // Agent defaults cannot hide a broken execution-profile policy implementation.
        let agents = std::fs::read_to_string(fixture.path("agents.toml"))
            .unwrap()
            .replace("project = 'demo'\nworkspace = 'task'\n", "");
        std::fs::write(fixture.path("agents.toml"), agents).unwrap();
        std::fs::write(fixture.path("profiles.toml"), "[profiles.inline]\nagents = ['coder']\nprojects = ['demo']\n[profiles.inline.workspaces.demo]\npolicy = 'worktree'\n[profiles.reference]\nagents = ['coder']\nprojects = ['demo']\n[profiles.reference.workspaces.demo]\ntemplate = 'task'\n").unwrap();
        std::fs::write(
            fixture.path("settings.toml"),
            "[profiles.inline.workspaces.demo]\nbase_ref = 'main'\n",
        )
        .unwrap();
        std::fs::write(fixture.path("hive.toml"), "include = ['settings.toml','profiles.toml','agents.toml','projects.toml','policies.toml']\n").unwrap();
        let (address, mut host) = fixture.start().await;
        for transport in ["ws", "http"] {
            Client
                .v2()
                .connect_with(
                    HttpClient::new(format!("{transport}://{address}")).unwrap(),
                    async |cx| {
                        let peer = Peer::initialize(
                            cx.clone(),
                            v2::InitializeRequest::new(
                                ProtocolVersion::V2,
                                v2::Implementation::new("named-profile-test", "1"),
                            ),
                        )
                        .await?;
                        let page = peer
                            .list_profiles(ListParams {
                                limit: Some(1),
                                cursor: None,
                            })
                            .await?;
                        assert_eq!(page.profiles[0].id, "inline");
                        let page = peer
                            .list_profiles(ListParams {
                                limit: Some(1),
                                cursor: page.next_cursor,
                            })
                            .await?;
                        assert_eq!(page.profiles[0].id, "reference");
                        assert!(page.next_cursor.is_none());
                        for profile in ["inline", "reference"] {
                            let opened = peer
                                .start_profile(
                                    profile,
                                    None,
                                    v2::NewSessionRequest::new(fixture.path("placeholder")),
                                )
                                .await?;
                            let context = &opened.meta.as_ref().unwrap()["hive"]["workspace"];
                            assert_eq!(context["project"], "demo");
                            let cwd = PathBuf::from(context["effectiveCwd"].as_str().unwrap());
                            assert!(cwd.join(".git").is_file());
                            fixture.assert_downstream_cwd(&cwd);
                            cx.send_request(
                                v2::CloseSessionRequest::new(opened.session_id).meta(opened.meta),
                            )
                            .block_task()
                            .await?;
                        }
                        let before = inventory(address).await["workspaces"]
                            .as_array()
                            .unwrap()
                            .len();
                        let error = peer
                            .start_profile(
                                "inline",
                                Some("outside"),
                                v2::NewSessionRequest::new(fixture.path("placeholder")),
                            )
                            .await
                            .unwrap_err();
                        assert_eq!(error.data.unwrap()["code"], "hive::remote::invalid_profile");
                        assert_eq!(
                            inventory(address).await["workspaces"]
                                .as_array()
                                .unwrap()
                                .len(),
                            before
                        );
                        Ok(())
                    },
                )
                .await
                .unwrap();
        }
        host.kill().await.unwrap();
        host.wait().await.unwrap();
    }
}

#[tokio::test]
async fn workspace_worktrees_reach_v1_from_composed_config() {
    check_composed_worktrees(AgentVersion::V1, false).await;
}

#[tokio::test]
async fn workspace_worktrees_reach_v1_from_reversed_composed_config() {
    check_composed_worktrees(AgentVersion::V1, true).await;
}

#[tokio::test]
async fn workspace_worktrees_reach_v2_from_composed_config() {
    check_composed_worktrees(AgentVersion::V2, false).await;
}

#[tokio::test]
async fn workspace_worktrees_reach_v2_from_reversed_composed_config() {
    check_composed_worktrees(AgentVersion::V2, true).await;
}

async fn check_composed_worktrees(version: AgentVersion, reverse: bool) {
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
                        assert_eq!(context.as_object().unwrap().len(), 4);
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

#[tokio::test]
async fn workspace_jj_projects_reach_both_acp_adapters_with_native_revision_and_stable_context() {
    for version in [AgentVersion::V1, AgentVersion::V2] {
        let fixture = Fixture::new(version, false);
        std::fs::write(fixture.path("projects.toml"), "[projects.demo]\nname = 'Demo'\nscm = 'jj'\nrepository_source = 'https://example.test/project'\ndefault_revision = 'main@origin'\n").unwrap();
        // Requiring WorkTrunk catches any accidental Git-only integration selection/probe.
        std::fs::write(
            fixture.path("policies.toml"),
            "[workspaces.task]\npolicy = 'jj'\n[worktrunk]\nenabled = true\n",
        )
        .unwrap();
        std::fs::write(fixture.path("jj.toml"), "[user]\nname = 'Hive Test'\nemail = 'hive@example.test'\n[signing]\nbehavior = 'drop'\n").unwrap();
        let (address, mut host) = fixture.start().await;
        timeout(Duration::from_secs(30), Client.v2().connect_with(
            HttpClient::new(format!("ws://{address}")).unwrap(), async |cx| {
                cx.send_request(v2::InitializeRequest::new(ProtocolVersion::V2, v2::Implementation::new("jj-workspace-test", "1"))).block_task().await?;
                let request = v2::NewSessionRequest::new(fixture.path("placeholder")).meta(selector(json!({"kind":"new","project":"demo","policy":"task","revision":"latest(main@origin)"})));
                let opened = cx.send_request(request).block_task().await?;
                let context = &opened.meta.as_ref().unwrap()["hive"]["workspace"];
                assert_eq!(context.as_object().unwrap().len(), 4);
                let cwd = PathBuf::from(context["effectiveCwd"].as_str().unwrap());
                assert!(cwd.join(".jj").is_dir());
                assert!(!cwd.join(".git").exists());
                assert_eq!(std::fs::read_to_string(cwd.join("committed.txt")).unwrap(), "workspace seed\n");
                fixture.assert_downstream_cwd(&cwd);
                let resources = inventory(address).await;
                let entry = &resources["workspaces"][0];
                assert_eq!(entry["selection"]["provisioner"], "jj.workspace");
                assert_eq!(entry["selection"]["repository"]["backend"], "jj");
                let blocked = reqwest::Client::new().post(format!("http://{address}/v1/workspaces/{}/remove", context["workspace"].as_str().unwrap())).json(&json!({"operation":hive_core::identity::OperationId::new(),"discard_changes":true})).send().await.unwrap();
                assert!(!blocked.status().is_success());
                cx.send_request(v2::CloseSessionRequest::new(opened.session_id).meta(opened.meta.clone())).block_task().await?;
                let attached = cx.send_request(v2::NewSessionRequest::new(fixture.path("placeholder")).meta(selector(json!({"kind":"attach","id":context["workspace"]})))).block_task().await?;
                assert_eq!(&attached.meta.as_ref().unwrap()["hive"]["workspace"], context);
                cx.send_request(v2::CloseSessionRequest::new(attached.session_id).meta(attached.meta)).block_task().await?;
                Ok(())
            },
        )).await.unwrap().unwrap();
        host.kill().await.unwrap();
        host.wait().await.unwrap();
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
