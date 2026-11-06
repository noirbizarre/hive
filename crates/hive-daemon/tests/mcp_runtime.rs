//! Real subprocess evidence complements the pure identity contract tests.

use hive_core::{
    identity::NodeId,
    workspace::{NodePath, PathSyntax},
};
use hive_daemon::mcp::{
    Credentials, Initialization, InstanceIdentity, Lifecycle, Ownership, ResolvedTransport, Scope,
    ScopeContext, SecurityContext, ServerDefinition, Transport,
    runtime::{McpRequest, Runtime, RuntimeError, RuntimePolicy},
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, time::Duration};

struct Fixture {
    directory: tempfile::TempDir,
    node: NodeId,
    runtime: Runtime,
}

impl Fixture {
    fn admission(
        &self,
        scope: Scope,
        cwd: Option<&std::path::Path>,
        idle: u64,
    ) -> hive_daemon::mcp::StdioAdmission {
        let path = |path: &std::path::Path| {
            NodePath::new(
                self.node.clone(),
                if cfg!(windows) {
                    PathSyntax::Windows
                } else {
                    PathSyntax::Posix
                },
                path.to_str().unwrap().into(),
            )
            .unwrap()
        };
        hive_daemon::mcp::StdioAdmission {
            definition_id: "fixture".into(),
            definition: ServerDefinition::new(
                Transport::Stdio {
                    command: env!("CARGO_BIN_EXE_hive-test-mcp").into(),
                    args: vec![self.log().to_str().unwrap().into(), "normal".into()],
                    env: BTreeMap::new(),
                },
                scope,
                Ownership::Managed,
                Lifecycle::OnDemand {
                    idle_timeout_seconds: idle,
                },
            )
            .unwrap(),
            launch: hive_daemon::mcp::StdioLaunch {
                executable: path(std::path::Path::new(env!("CARGO_BIN_EXE_hive-test-mcp"))),
                env: std::env::vars_os().collect(),
                cwd: cwd.map(path),
            },
            initialization: Initialization {
                protocol_version: "2025-11-25".into(),
                client_info: json!({"name":"hive-test", "version":"1"}),
                capabilities: json!({}),
                options: json!({}),
                roots: vec![],
            },
            security: SecurityContext {
                principal: "test".into(),
                partition: "test".into(),
                policy_generation: "1".into(),
                credentials: Credentials::Complete {
                    external_generations: BTreeMap::new(),
                },
            },
            sharing: hive_daemon::mcp::Sharing::Shared,
        }
    }

    fn new() -> Self {
        Self::with_policy(RuntimePolicy {
            // Successful native startup needs the production budget on loaded CI hosts.
            startup_timeout: Duration::from_secs(15),
            // These are upper bounds, not sleeps: loaded Windows hosts still need time to reap.
            shutdown_grace: Duration::from_secs(2),
            exit_grace: Duration::from_secs(2),
            restart_backoff: Duration::from_millis(10),
            ..Default::default()
        })
    }

    fn with_policy(policy: RuntimePolicy) -> Self {
        let node = NodeId::new();
        Self {
            directory: tempfile::tempdir().unwrap(),
            runtime: Runtime::new(node.clone(), policy).unwrap(),
            node,
        }
    }

    fn log(&self) -> std::path::PathBuf {
        self.directory.path().join("events.jsonl")
    }

    fn identity(&self, mode: &str, idle: u64) -> InstanceIdentity {
        let missing = self.directory.path().join("missing-executable");
        let executable = if mode == "missing" {
            missing.to_str().unwrap()
        } else {
            env!("CARGO_BIN_EXE_hive-test-mcp")
        };
        let args = vec![self.log().to_str().unwrap().into(), mode.into()];
        let definition = ServerDefinition::new(
            Transport::Stdio {
                command: executable.into(),
                args: args.clone(),
                env: BTreeMap::new(),
            },
            Scope::Machine,
            Ownership::Managed,
            Lifecycle::OnDemand {
                idle_timeout_seconds: idle,
            },
        )
        .unwrap();
        let path = |path: &str| {
            NodePath::new(
                self.node.clone(),
                if cfg!(windows) {
                    PathSyntax::Windows
                } else {
                    PathSyntax::Posix
                },
                path.into(),
            )
            .unwrap()
        };
        let construct = if mode == "private" {
            InstanceIdentity::private
        } else {
            InstanceIdentity::shared
        };
        construct(
            "fixture".into(),
            definition,
            ScopeContext {
                node: self.node.clone(),
                project: None,
                workspace: None,
            },
            ResolvedTransport::Stdio {
                executable: path(executable),
                args,
                env: std::env::vars_os().collect(),
                cwd: path(self.directory.path().to_str().unwrap()),
            },
            Initialization {
                protocol_version: "2025-11-25".into(),
                client_info: json!({"name": "hive-test", "version": "1"}),
                capabilities: if mode == "roots" {
                    json!({"roots": {}})
                } else {
                    json!({})
                },
                options: json!({"custom": "seed-secret"}),
                roots: if mode == "roots" {
                    vec![path(
                        self.directory.path().join("a space").to_str().unwrap(),
                    )]
                } else {
                    vec![]
                },
            },
            SecurityContext {
                principal: "test".into(),
                partition: "test".into(),
                policy_generation: "1".into(),
                credentials: if mode == "private" {
                    Credentials::Unknown
                } else {
                    Credentials::Complete {
                        external_generations: BTreeMap::new(),
                    }
                },
            },
        )
        .unwrap()
    }

    fn events(&self) -> Vec<Value> {
        std::fs::read_to_string(self.log())
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    async fn observed(&self, predicate: impl Fn(&[Value]) -> bool) {
        tokio::time::timeout(Duration::from_secs(20), async {
            while !predicate(&self.events()) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("fixture did not reach protocol barrier");
    }
}

#[tokio::test]
async fn scoped_admission_shares_canonical_worktrees_and_isolates_other_worktrees_projects_and_credentials()
 {
    use agent_client_protocol::{
        Client,
        schema::{ProtocolVersion, v2},
    };
    use agent_client_protocol_http::HttpClient;
    let mut fixture = Fixture::new();
    // Git for Windows rejects canonicalization's verbatim prefix in worktree destination arguments.
    let base = fixture.directory.path().to_path_buf();
    let first = base.join("first");
    let second = base.join("second");
    let other = base.join("other");
    std::fs::create_dir(&first).unwrap();
    std::fs::create_dir(&other).unwrap();
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(&first)
            .args(["-c", "core.hooksPath=", "-c", "commit.gpgsign=false"])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init", "--initial-branch=main"]);
    git(&[
        "-c",
        "user.name=Hive Test",
        "-c",
        "user.email=hive@example.test",
        "commit",
        "--allow-empty",
        "-m",
        "seed",
    ]);
    git(&["worktree", "add", "-b", "linked", second.to_str().unwrap()]);
    std::fs::create_dir(first.join("a")).unwrap();
    std::fs::create_dir(first.join("b")).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(first.join("b"), base.join("alias")).unwrap();
    let alias = if cfg!(unix) {
        base.join("alias")
    } else {
        first.join("b")
    };
    let config = serde_json::from_value(json!({
        "default_agent":"mock", "projects":{"demo":{"name":"Demo"},"other":{"name":"Other"}},
        "workspaces":{"first":{"policy":"existing","directory":first},"second":{"policy":"existing","directory":second},"other":{"policy":"existing","directory":other}},
        "agents":{"mock":{"command":env!("CARGO_BIN_EXE_hive-test-agent"),"versions":["v2"]}}
    })).unwrap();
    let broker =
        hive_daemon::Broker::persistent(config, base.join("state/catalog.sqlite3")).unwrap();
    fixture.node = broker.mcp_execution_node().await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = hive_daemon::router(broker.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let owner = broker.clone();
    Client
        .v2()
        .connect_with(
            HttpClient::new(format!("ws://{address}")).unwrap(),
            async |cx| {
                cx.send_request(v2::InitializeRequest::new(
                    ProtocolVersion::V2,
                    v2::Implementation::new("scope-test", "1"),
                ))
                .block_task()
                .await?;
                let mut registered = Vec::new();
                for (policy, project) in [("first", "demo"), ("second", "demo"), ("other", "other")]
                {
                    let mut meta = v2::Meta::new();
                    meta.insert(
                        "hive".into(),
                        json!({"workspace":{"kind":"new","project":project,"policy":policy}}),
                    );
                    registered.push(
                        cx.send_request(v2::NewSessionRequest::new(base.clone()).meta(meta))
                            .block_task()
                            .await?,
                    );
                }
                let mut sessions = Vec::new();
                for cwd in [first.join("a"), alias] {
                    sessions.push(
                        cx.send_request(v2::NewSessionRequest::new(cwd))
                            .block_task()
                            .await?,
                    );
                }
                let a = owner
                    .bind_scoped_mcp(
                        &sessions[0].session_id,
                        fixture.admission(Scope::Workspace, None, 60),
                    )
                    .await
                    .unwrap();
                let b = owner
                    .bind_scoped_mcp(
                        &sessions[1].session_id,
                        fixture.admission(Scope::Workspace, None, 60),
                    )
                    .await
                    .unwrap();
                let pid = |binding: hive_daemon::mcp::SessionMcpLease| async move {
                    binding
                        .request(tool("pid", json!({})))
                        .unwrap()
                        .result()
                        .await
                        .unwrap()["pid"]
                        .clone()
                };
                let original = pid(a.clone()).await;
                assert_eq!(original, pid(b.clone()).await);
                let linked = owner
                    .bind_scoped_mcp(
                        &registered[1].session_id,
                        fixture.admission(Scope::Workspace, None, 60),
                    )
                    .await
                    .unwrap();
                assert_ne!(original, pid(linked).await);
                let distinct = owner
                    .bind_scoped_mcp(
                        &registered[2].session_id,
                        fixture.admission(Scope::Workspace, None, 60),
                    )
                    .await
                    .unwrap();
                assert_ne!(original, pid(distinct).await);
                // Project/machine context is explicit and identical, rather than accidentally using each session cwd.
                for scope in [Scope::Project, Scope::Machine] {
                    let x = owner
                        .bind_scoped_mcp(
                            &registered[0].session_id,
                            fixture.admission(scope, Some(&base), 60),
                        )
                        .await
                        .unwrap();
                    let y = owner
                        .bind_scoped_mcp(
                            &registered[1].session_id,
                            fixture.admission(scope, Some(&base), 60),
                        )
                        .await
                        .unwrap();
                    assert_eq!(pid(x.clone()).await, pid(y).await);
                    let z = owner
                        .bind_scoped_mcp(
                            &registered[2].session_id,
                            fixture.admission(scope, Some(&base), 60),
                        )
                        .await
                        .unwrap();
                    assert_eq!(
                        pid(x.clone()).await == pid(z).await,
                        scope == Scope::Machine
                    );
                }
                let plain = cx
                    .send_request(v2::NewSessionRequest::new(base.clone()))
                    .block_task()
                    .await?;
                assert!(matches!(
                    owner
                        .bind_scoped_mcp(
                            &plain.session_id,
                            fixture.admission(Scope::Workspace, None, 60)
                        )
                        .await,
                    Err(hive_daemon::mcp::AdmissionError::Scope)
                ));
                let machine = owner
                    .bind_scoped_mcp(
                        &plain.session_id,
                        fixture.admission(Scope::Machine, Some(&base), 60),
                    )
                    .await
                    .unwrap();
                assert!(pid(machine).await.is_number());
                let before = fixture
                    .events()
                    .iter()
                    .filter(|event| event["event"] == "spawn")
                    .count();
                let mut cancelled = fixture.admission(Scope::Workspace, None, 60);
                cancelled.definition_id = "cancelled-admission".into();
                let mut future =
                    Box::pin(owner.bind_scoped_mcp(&sessions[1].session_id, cancelled));
                // Poll exactly once, then cancel before the coordinator can acquire a lease.
                std::future::poll_fn(|cx| {
                    use std::future::Future;
                    assert!(future.as_mut().poll(cx).is_pending());
                    std::task::Poll::Ready(())
                })
                .await;
                drop(future);
                let (duplicate, concurrent) = tokio::join!(
                    owner.bind_scoped_mcp(
                        &sessions[1].session_id,
                        fixture.admission(Scope::Workspace, None, 60)
                    ),
                    owner.bind_scoped_mcp(
                        &sessions[1].session_id,
                        fixture.admission(Scope::Workspace, None, 60)
                    ),
                );
                assert_eq!(b.id(), duplicate.unwrap().id());
                assert_eq!(b.id(), concurrent.unwrap().id());
                assert_eq!(
                    before,
                    fixture
                        .events()
                        .iter()
                        .filter(|event| event["event"] == "spawn")
                        .count()
                );
                let mut unknown = fixture.admission(Scope::Workspace, None, 60);
                unknown.security.credentials = Credentials::Unknown;
                assert!(matches!(
                    owner
                        .bind_scoped_mcp(&sessions[1].session_id, unknown)
                        .await,
                    Err(hive_daemon::mcp::AdmissionError::Contract(
                        hive_daemon::mcp::McpError::Security
                    ))
                ));
                let mut foreign = fixture.admission(Scope::Workspace, None, 60);
                foreign.launch.executable = NodePath::new(
                    NodeId::new(),
                    foreign.launch.executable.syntax(),
                    foreign.launch.executable.as_str().into(),
                )
                .unwrap();
                assert!(matches!(
                    owner
                        .bind_scoped_mcp(&sessions[1].session_id, foreign)
                        .await,
                    Err(hive_daemon::mcp::AdmissionError::Path)
                ));
                cx.send_request(
                    v2::CloseSessionRequest::new(sessions[0].session_id.clone())
                        .meta(sessions[0].meta.clone()),
                )
                .block_task()
                .await?;
                assert_eq!(original, pid(b.clone()).await);
                owner.release_mcp(&sessions[1].session_id, b.id());
                let mut rotated = fixture.admission(Scope::Workspace, None, 60);
                rotated.security.credentials = Credentials::Complete {
                    external_generations: BTreeMap::from([("keychain".into(), "rotated".into())]),
                };
                let rotated = owner
                    .bind_scoped_mcp(&sessions[1].session_id, rotated)
                    .await
                    .unwrap();
                assert_ne!(original, pid(rotated).await);
                let mut racing = fixture.admission(Scope::Workspace, None, 60);
                racing.definition_id = "close-race".into();
                let (admitted, closed) = tokio::join!(
                    owner.bind_scoped_mcp(&sessions[1].session_id, racing),
                    cx.send_request(
                        v2::CloseSessionRequest::new(sessions[1].session_id.clone())
                            .meta(sessions[1].meta.clone())
                    )
                    .block_task(),
                );
                closed?;
                if let Ok(binding) = admitted {
                    assert!(matches!(
                        binding.request(tool("pid", json!({}))),
                        Err(RuntimeError::Binding)
                    ));
                }
                Ok(())
            },
        )
        .await
        .unwrap();
    broker.shutdown().await;
    server.abort();
}

fn tool(name: &str, arguments: Value) -> McpRequest {
    McpRequest::new("tools/call", json!({"name": name, "arguments": arguments})).unwrap()
}

#[cfg(target_os = "linux")]
async fn terminated(pid: u64) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                Err(error) => panic!("cannot inspect fixture process {pid}: {error}"),
                // An orphan zombie has terminated; only the system reaper can collect it.
                Ok(stat)
                    if stat
                        .rsplit_once(") ")
                        .is_some_and(|(_, state)| state.starts_with('Z')) =>
                {
                    break;
                }
                _ => tokio::time::sleep(Duration::from_millis(5)).await,
            }
        }
    })
    .await
    .expect("fixture process survived connection cleanup");
}

#[tokio::test]
async fn equal_leases_share_one_initialization_and_correlate_reversed_responses() {
    let fixture = Fixture::new();
    let identity = fixture.identity("normal", 0);
    let first = fixture.runtime.acquire(identity.clone()).unwrap();
    let second = fixture.runtime.acquire(identity).unwrap();
    let a = first.request(tool("hold", json!({"caller": "a"})));
    let b = second.request(tool("hold", json!({"caller": "b"})));
    fixture
        .observed(|events| {
            events
                .iter()
                .filter(|event| event["params"]["name"] == "hold")
                .count()
                == 2
        })
        .await;
    second
        .request(tool("release", json!({})))
        .result()
        .await
        .unwrap();
    assert_eq!(a.result().await.unwrap(), json!({"caller": "a"}));
    assert_eq!(b.result().await.unwrap(), json!({"caller": "b"}));
    assert_eq!(
        fixture
            .events()
            .iter()
            .filter(|event| event["event"] == "spawn")
            .count(),
        1
    );
    assert_eq!(
        fixture
            .events()
            .iter()
            .filter(|event| event["method"] == "initialize")
            .count(),
        1
    );
    drop(first);
    second
        .request(McpRequest::new("ping", json!({})).unwrap())
        .result()
        .await
        .unwrap();
    drop(second);
    fixture
        .observed(|events| events.iter().any(|event| event["event"] == "eof"))
        .await;
    fixture.runtime.shutdown().await;
}

#[tokio::test]
async fn ignored_cancellation_keeps_operation_alive_and_does_not_interrupt_other_callers() {
    let fixture = Fixture::new();
    let identity = fixture.identity("normal", 0);
    let lease = fixture.runtime.acquire(identity.clone()).unwrap();
    let call = lease.request(tool("hold", json!({"late": true})));
    fixture
        .observed(|events| events.iter().any(|event| event["params"]["name"] == "hold"))
        .await;
    call.cancel();
    assert_eq!(call.result().await, Err(RuntimeError::Cancelled));
    fixture
        .observed(|events| {
            events
                .iter()
                .any(|event| event["method"] == "notifications/cancelled")
        })
        .await;
    drop(lease);
    let survivor = fixture.runtime.acquire(identity).unwrap();
    survivor
        .request(tool("release", json!({})))
        .result()
        .await
        .unwrap();
    assert_eq!(
        fixture
            .events()
            .iter()
            .filter(|event| event["event"] == "spawn")
            .count(),
        1
    );
    assert!(
        !fixture
            .events()
            .iter()
            .any(|event| event["method"] == "$/cancel_request")
    );
    fixture.runtime.shutdown().await;
}

#[tokio::test]
async fn cancellation_before_dispatch_sends_no_tool_request() {
    let fixture = Fixture::new();
    let lease = fixture
        .runtime
        .acquire(fixture.identity("normal", 0))
        .unwrap();
    let call = lease.request(tool("echo", json!({})));
    call.cancel();
    assert_eq!(call.result().await, Err(RuntimeError::Cancelled));
    lease
        .request(McpRequest::new("ping", json!({})).unwrap())
        .result()
        .await
        .unwrap();
    assert!(
        !fixture
            .events()
            .iter()
            .any(|event| event["method"] == "tools/call")
    );
    fixture.runtime.shutdown().await;
}

#[tokio::test]
async fn task_augmented_tool_calls_are_rejected_locally_without_a_wire_request() {
    let fixture = Fixture::new();
    let lease = fixture
        .runtime
        .acquire(fixture.identity("normal", 0))
        .unwrap();
    lease
        .request(McpRequest::new("ping", json!({})).unwrap())
        .result()
        .await
        .unwrap();
    assert!(matches!(
        McpRequest::new(
            "tools/call",
            json!({"name": "echo", "arguments": {}, "task": {"ttl": 60000}}),
        ),
        Err(RuntimeError::Request)
    ));
    // A completed ping fences the log inspection without a timing-based absence check.
    lease
        .request(McpRequest::new("ping", json!({})).unwrap())
        .result()
        .await
        .unwrap();
    assert!(
        !fixture
            .events()
            .iter()
            .any(|event| event["method"] == "tools/call")
    );
    fixture.runtime.shutdown().await;
}

#[tokio::test]
async fn oversized_local_arguments_leave_unrelated_calls_and_the_process_alive() {
    let fixture = Fixture::with_policy(RuntimePolicy {
        max_frame_bytes: 4096,
        shutdown_grace: Duration::from_millis(50),
        exit_grace: Duration::from_millis(100),
        ..Default::default()
    });
    let lease = fixture
        .runtime
        .acquire(fixture.identity("normal", 0))
        .unwrap();
    let pid = lease
        .request(tool("echo", json!({})))
        .result()
        .await
        .unwrap()["pid"]
        .clone();
    let survivor = lease.request(tool("hold", json!({"survivor": true})));
    fixture
        .observed(|events| events.iter().any(|event| event["params"]["name"] == "hold"))
        .await;
    let rejected = tokio::time::timeout(
        Duration::from_secs(5),
        lease
            .request(tool(
                "oversized-local",
                json!({"payload": "x".repeat(8192)}),
            ))
            .result(),
    )
    .await
    .expect("oversized local request did not settle");
    // The public typed error is evolving; survival and wire absence prove local rejection.
    assert!(rejected.is_err());
    tokio::time::timeout(
        Duration::from_secs(5),
        lease.request(tool("release", json!({}))).result(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), survivor.result())
            .await
            .unwrap()
            .unwrap(),
        json!({"survivor": true})
    );
    assert_eq!(
        lease
            .request(tool("echo", json!({})))
            .result()
            .await
            .unwrap()["pid"],
        pid
    );
    let events = fixture.events();
    assert!(
        !events
            .iter()
            .any(|event| event["params"]["name"] == "oversized-local")
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event["event"] == "spawn")
            .count(),
        1
    );
    fixture.runtime.shutdown().await;
}

#[tokio::test]
async fn conflicting_response_arms_fail_the_connection_and_clean_up_the_live_server() {
    let fixture = Fixture::new();
    let lease = fixture
        .runtime
        .acquire(fixture.identity("normal", 0))
        .unwrap();
    // Successful application work proves this malformed frame arrives after initialization.
    let pid = lease
        .request(tool("echo", json!({})))
        .result()
        .await
        .unwrap()["pid"]
        .as_u64()
        .unwrap();
    let held = lease.request(tool("hold", json!({})));
    fixture
        .observed(|events| events.iter().any(|event| event["params"]["name"] == "hold"))
        .await;
    assert_eq!(
        tokio::time::timeout(
            Duration::from_secs(5),
            lease
                .request(tool("ambiguous-response", json!({})))
                .result(),
        )
        .await
        .expect("conflicting response did not fail the connection"),
        Err(RuntimeError::Connection)
    );
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), held.result())
            .await
            .expect("connection failure left an unrelated caller pending"),
        Err(RuntimeError::Connection)
    );
    // Observe autonomous cleanup before shutdown can mask a leaked generation.
    #[cfg(target_os = "linux")]
    terminated(pid).await;
    #[cfg(not(target_os = "linux"))]
    let _ = pid;
    fixture.runtime.shutdown().await;
}

#[tokio::test]
async fn crashed_calls_are_not_replayed_and_later_work_lazily_restarts() {
    let fixture = Fixture::with_policy(RuntimePolicy {
        // One slot makes any retry before settlement observable instead of relying on load.
        max_concurrent_calls: 1,
        restart_backoff: Duration::from_millis(10),
        ..Default::default()
    });
    let lease = fixture
        .runtime
        .acquire(fixture.identity("normal", 0))
        .unwrap();
    let crashed = lease.request(tool("crash", json!({})));
    let settled = crashed.settlement();
    assert!(matches!(
        crashed.result().await,
        Err(RuntimeError::Connection)
    ));
    wait_for_settlement(settled).await;
    fixture
        .observed(|events| {
            events
                .iter()
                .any(|event| event["params"]["name"] == "crash")
        })
        .await;
    // The policy's explicit backoff is observed rather than guessing process startup timing.
    let result = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let call = lease.request(tool("echo", json!({})));
            let settled = call.settlement();
            let result = call.result().await;
            // Failure delivery does not release admission; yielding can accumulate pending cleanup.
            wait_for_settlement(settled).await;
            match result {
                Err(RuntimeError::Backoff { retry_after_ms }) => {
                    tokio::time::sleep(Duration::from_millis(retry_after_ms.max(1))).await
                }
                Err(RuntimeError::Connection) => continue,
                result => break result,
            }
        }
    })
    .await
    .unwrap()
    .unwrap();
    assert!(result["pid"].is_number());
    assert_eq!(
        fixture
            .events()
            .iter()
            .filter(|event| event["event"] == "spawn")
            .count(),
        2
    );
    assert_eq!(
        fixture
            .events()
            .iter()
            .filter(|event| event["params"]["name"] == "crash")
            .count(),
        1
    );
    fixture.runtime.shutdown().await;
}

async fn wait_for_settlement(mut settled: tokio::sync::watch::Receiver<bool>) {
    tokio::time::timeout(Duration::from_secs(20), async {
        while !*settled.borrow_and_update() {
            settled
                .changed()
                .await
                .expect("operation ended without settlement");
        }
    })
    .await
    .expect("MCP operation did not release its admission slot");
}

#[tokio::test]
async fn initialization_failures_and_stderr_floods_do_not_hang_the_runtime() {
    for mode in ["version", "hang", "exit", "stderr"] {
        let fixture = if mode == "hang" {
            Fixture::with_policy(RuntimePolicy {
                // Only the deliberate non-responder should use an abbreviated startup budget.
                startup_timeout: Duration::from_secs(1),
                shutdown_grace: Duration::from_millis(50),
                exit_grace: Duration::from_millis(100),
                ..Default::default()
            })
        } else {
            Fixture::new()
        };
        let lease = fixture.runtime.acquire(fixture.identity(mode, 0)).unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(20),
            lease
                .request(McpRequest::new("ping", json!({})).unwrap())
                .result(),
        )
        .await
        .unwrap();
        if mode == "stderr" {
            assert!(result.is_ok());
        } else {
            assert!(result.is_err());
        }
        fixture.runtime.shutdown().await;
    }
}

#[tokio::test]
async fn shutdown_settles_ignored_cancellation_and_rejects_future_admission() {
    let fixture = Fixture::with_policy(RuntimePolicy {
        // Deliberately unresponsive work should hit a short grace, not spend the production delay.
        shutdown_grace: Duration::from_millis(50),
        ..Default::default()
    });
    let identity = fixture.identity("normal", 0);
    let lease = fixture.runtime.acquire(identity.clone()).unwrap();
    let call = lease.request(tool("hold", json!({})));
    fixture
        .observed(|events| events.iter().any(|event| event["params"]["name"] == "hold"))
        .await;
    tokio::time::timeout(Duration::from_secs(3), fixture.runtime.shutdown())
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(1), call.result())
            .await
            .unwrap()
            .is_err()
    );
    assert!(matches!(
        fixture.runtime.acquire(identity),
        Err(RuntimeError::Stopping)
    ));
}

#[tokio::test]
async fn broker_sessions_share_durable_bindings_across_disconnect_and_close() {
    use agent_client_protocol::{
        Client,
        schema::{ProtocolVersion, v2},
    };
    use agent_client_protocol_http::HttpClient;

    let mut fixture = Fixture::new();
    let config = serde_json::from_value(json!({"default_agent": "mock", "agents": {"mock": {"command": env!("CARGO_BIN_EXE_hive-test-agent"), "versions": ["v2"]}}})).unwrap();
    let broker = hive_daemon::Broker::new(config).unwrap();
    fixture.node = broker.mcp_execution_node().await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = hive_daemon::router(broker.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let (saved, received) = tokio::sync::oneshot::channel();
    let owner = broker.clone();
    let identity = fixture.identity("normal", 0);
    let cwd = fixture.directory.path().to_path_buf();
    Client
        .v2()
        .connect_with(
            HttpClient::new(format!("ws://{address}")).unwrap(),
            async move |cx| {
                cx.send_request(v2::InitializeRequest::new(
                    ProtocolVersion::V2,
                    v2::Implementation::new("mcp-owner-test", "1"),
                ))
                .block_task()
                .await?;
                let first = cx
                    .send_request(v2::NewSessionRequest::new(cwd.clone()))
                    .block_task()
                    .await?;
                let second = cx
                    .send_request(v2::NewSessionRequest::new(cwd))
                    .block_task()
                    .await?;
                let a = owner
                    .bind_mcp(&first.session_id, identity.clone())
                    .await
                    .unwrap();
                let b = owner
                    .bind_mcp(&second.session_id, identity.clone())
                    .await
                    .unwrap();
                assert_eq!(
                    owner
                        .bind_mcp(&first.session_id, identity)
                        .await
                        .unwrap()
                        .id(),
                    a.id()
                );
                let pid = a
                    .request(tool("echo", json!({})))
                    .unwrap()
                    .result()
                    .await
                    .unwrap()["pid"]
                    .clone();
                assert_eq!(
                    b.request(tool("echo", json!({})))
                        .unwrap()
                        .result()
                        .await
                        .unwrap()["pid"],
                    pid
                );
                let held = a.request(tool("hold", json!({"closed": true}))).unwrap();
                // Same connection's ping is a protocol barrier after the published hold.
                b.request(McpRequest::new("ping", json!({})).unwrap())
                    .unwrap()
                    .result()
                    .await
                    .unwrap();
                cx.send_request(v2::CloseSessionRequest::new(first.session_id).meta(first.meta))
                    .block_task()
                    .await?;
                assert_eq!(held.result().await, Err(RuntimeError::Cancelled));
                assert!(matches!(
                    a.request(tool("echo", json!({}))),
                    Err(RuntimeError::Binding)
                ));
                assert_eq!(
                    b.request(tool("echo", json!({})))
                        .unwrap()
                        .result()
                        .await
                        .unwrap()["pid"],
                    pid
                );
                b.request(tool("release", json!({})))
                    .unwrap()
                    .result()
                    .await
                    .unwrap();
                saved
                    .send((second.session_id, b, pid))
                    .unwrap_or_else(|_| panic!("test receiver dropped"));
                Ok(())
            },
        )
        .await
        .unwrap();
    let (session, binding, pid) = received.await.unwrap();
    // The ACP connection has ended; durable session ownership remains usable.
    assert_eq!(
        binding
            .request(tool("echo", json!({})))
            .unwrap()
            .result()
            .await
            .unwrap()["pid"],
        pid
    );
    broker.release_mcp(&session, binding.id());
    assert!(matches!(
        binding.request(tool("echo", json!({}))),
        Err(RuntimeError::Binding)
    ));
    fixture
        .observed(|events| events.iter().any(|event| event["event"] == "eof"))
        .await;
    assert_eq!(
        fixture
            .events()
            .iter()
            .filter(|event| event["event"] == "spawn")
            .count(),
        1
    );
    broker.shutdown().await;
    server.abort();
}

#[tokio::test]
async fn private_admissions_and_different_effective_definitions_do_not_share() {
    let fixture = Fixture::new();
    let mut leases = Vec::new();
    let mut pids = std::collections::BTreeSet::new();
    for (mode, idle) in [("private", 0), ("private", 0), ("normal", 0), ("normal", 1)] {
        let lease = fixture
            .runtime
            .acquire(fixture.identity(mode, idle))
            .unwrap();
        let result = lease
            .request(tool("echo", json!({})))
            .result()
            .await
            .unwrap();
        pids.insert(result["pid"].as_u64().unwrap());
        leases.push(lease);
    }
    assert_eq!(pids.len(), 4);
    fixture.runtime.shutdown().await;
}

#[tokio::test]
async fn roots_are_immutable_file_uris_and_unsupported_callbacks_are_rejected() {
    let fixture = Fixture::new();
    let lease = fixture
        .runtime
        .acquire(fixture.identity("roots", 0))
        .unwrap();
    lease
        .request(tool("echo", json!({})))
        .result()
        .await
        .unwrap();
    fixture
        .observed(|events| {
            events
                .iter()
                .any(|event| event["id"] == "root-query" && event.get("result").is_some())
                && events
                    .iter()
                    .any(|event| event["id"] == "sampling-query" && event.get("error").is_some())
        })
        .await;
    let events = fixture.events();
    let roots = events
        .iter()
        .find(|event| event["id"] == "root-query" && event.get("result").is_some())
        .unwrap();
    let uri = roots["result"]["roots"][0]["uri"].as_str().unwrap();
    assert!(uri.starts_with("file:///"));
    assert!(uri.ends_with("a%20space"));
    let initialize = events
        .iter()
        .find(|event| event["method"] == "initialize")
        .unwrap();
    assert_eq!(initialize["params"]["custom"], "seed-secret");
    assert!(initialize["params"].get("roots").is_none());
    fixture.runtime.shutdown().await;
}

#[tokio::test]
async fn missing_executables_and_invalid_frames_produce_controlled_failures() {
    for mode in ["missing", "oversized", "malformed"] {
        let mut fixture = Fixture::new();
        fixture.runtime = Runtime::new(
            fixture.node.clone(),
            RuntimePolicy {
                max_frame_bytes: 1024,
                ..Default::default()
            },
        )
        .unwrap();
        let lease = fixture.runtime.acquire(fixture.identity(mode, 0)).unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(20),
            lease
                .request(McpRequest::new("ping", json!({})).unwrap())
                .result(),
        )
        .await
        .unwrap();
        assert!(matches!(
            result,
            Err(RuntimeError::Startup | RuntimeError::Initialize | RuntimeError::Backoff { .. })
        ));
        fixture.runtime.shutdown().await;
    }
}

#[tokio::test]
async fn verbose_sdk_logging_and_remote_errors_do_not_expose_payloads() {
    use std::sync::{Arc, Mutex};
    #[derive(Clone)]
    struct Capture(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let captured = bytes.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(move || Capture(captured.clone()))
        .finish();
    // A global test subscriber also sees independently spawned runtime tasks.
    tracing::subscriber::set_global_default(subscriber).unwrap();
    {
        let fixture = Fixture::new();
        let lease = fixture
            .runtime
            .acquire(fixture.identity("normal", 0))
            .unwrap();
        lease
            .request(tool("echo", json!({"token": "seed-secret"})))
            .result()
            .await
            .unwrap();
        let error = lease
            .request(tool("error", json!({})))
            .result()
            .await
            .unwrap_err();
        assert_eq!(error, RuntimeError::Remote { code: -12345 });
        assert!(!format!("{error:?} {error}").contains("seed-secret"));
        fixture.runtime.shutdown().await;
    }
    let output = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
    assert!(!output.contains("seed-secret"), "payload leaked: {output}");
}

#[tokio::test]
async fn startup_waiters_are_bounded_and_dropped_callers_do_not_release_server_slots() {
    let mut fixture = Fixture::new();
    fixture.runtime = Runtime::new(
        fixture.node.clone(),
        RuntimePolicy {
            max_concurrent_calls: 1,
            shutdown_grace: Duration::from_millis(20),
            exit_grace: Duration::from_millis(100),
            ..Default::default()
        },
    )
    .unwrap();
    let lease = fixture
        .runtime
        .acquire(fixture.identity("normal", 0))
        .unwrap();
    let held = lease.request(tool("hold", json!({})));
    // Reservation is synchronous, including while initialization is still pending.
    assert_eq!(
        lease.request(tool("echo", json!({}))).result().await,
        Err(RuntimeError::Busy)
    );
    fixture
        .observed(|events| events.iter().any(|event| event["params"]["name"] == "hold"))
        .await;
    drop(held);
    fixture
        .observed(|events| {
            events
                .iter()
                .any(|event| event["method"] == "notifications/cancelled")
        })
        .await;
    assert_eq!(
        lease.request(tool("echo", json!({}))).result().await,
        Err(RuntimeError::Busy)
    );
    fixture.runtime.shutdown().await;
}

#[tokio::test]
async fn crash_loops_are_bounded_even_when_a_binding_is_reacquired() {
    let fixture = Fixture::new();
    let identity = fixture.identity("exit", 0);
    let mut lease = fixture.runtime.acquire(identity.clone()).unwrap();
    for attempt in 0..3 {
        loop {
            match lease
                .request(McpRequest::new("ping", json!({})).unwrap())
                .result()
                .await
            {
                Err(RuntimeError::Backoff { retry_after_ms }) if retry_after_ms < 1000 => {
                    tokio::time::sleep(Duration::from_millis(retry_after_ms.max(1))).await
                }
                result => {
                    assert!(result.is_err());
                    break;
                }
            }
        }
        fixture
            .observed(|events| {
                events
                    .iter()
                    .filter(|event| event["event"] == "spawn")
                    .count()
                    == attempt + 1
            })
            .await;
        drop(lease);
        lease = fixture.runtime.acquire(identity.clone()).unwrap();
    }
    let error = lease
        .request(McpRequest::new("ping", json!({})).unwrap())
        .result()
        .await
        .unwrap_err();
    assert!(matches!(error, RuntimeError::Backoff { .. }));
    assert_eq!(
        fixture
            .events()
            .iter()
            .filter(|event| event["event"] == "spawn")
            .count(),
        3
    );
    fixture.runtime.shutdown().await;
}

#[tokio::test]
async fn leader_exit_does_not_wait_for_descendants_to_close_protocol_pipes() {
    let fixture = Fixture::new();
    let lease = fixture
        .runtime
        .acquire(fixture.identity("descendant", 0))
        .unwrap();
    assert!(
        tokio::time::timeout(
            Duration::from_secs(20),
            lease
                .request(McpRequest::new("ping", json!({})).unwrap())
                .result()
        )
        .await
        .unwrap()
        .is_err()
    );
    fixture.runtime.shutdown().await;
    #[cfg(target_os = "linux")]
    {
        let pid = std::fs::read_to_string(format!("{}.child", fixture.log().display())).unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
                    Err(_) => break,
                    // An orphan zombie has terminated; only the system reaper can collect it.
                    Ok(stat)
                        if stat
                            .rsplit_once(") ")
                            .is_some_and(|(_, state)| state.starts_with('Z')) =>
                    {
                        break;
                    }
                    _ => tokio::time::sleep(Duration::from_millis(5)).await,
                }
            }
        })
        .await
        .expect("descendant survived group termination");
    }
}

#[tokio::test]
async fn shutdown_fences_a_generation_that_has_not_finished_starting() {
    let fixture = Fixture::with_policy(RuntimePolicy {
        // Leave initialization pending throughout the observation and shutdown budgets.
        startup_timeout: Duration::from_secs(60),
        shutdown_grace: Duration::from_millis(50),
        exit_grace: Duration::from_millis(100),
        ..Default::default()
    });
    let lease = fixture
        .runtime
        .acquire(fixture.identity("hang", 0))
        .unwrap();
    let call = lease.request(McpRequest::new("ping", json!({})).unwrap());
    fixture
        .observed(|events| events.iter().any(|event| event["method"] == "initialize"))
        .await;
    tokio::time::timeout(Duration::from_secs(5), fixture.runtime.shutdown())
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(1), call.result())
            .await
            .unwrap()
            .is_err()
    );
}

#[tokio::test]
async fn worker_exit_retains_only_recoverable_session_ownership_and_allows_offline_release() {
    use agent_client_protocol::{
        Client,
        schema::{ProtocolVersion, v2},
    };
    use agent_client_protocol_http::HttpClient;
    for persistent in [false, true] {
        let mut fixture = Fixture::new();
        let config = serde_json::from_value(json!({"default_agent": "mock", "agents": {"mock": {"command": env!("CARGO_BIN_EXE_hive-test-agent"), "versions": ["v2"]}}})).unwrap();
        let broker = if persistent {
            hive_daemon::Broker::persistent(
                config,
                fixture.directory.path().join("state/catalog.sqlite3"),
            )
            .unwrap()
        } else {
            hive_daemon::Broker::new(config).unwrap()
        };
        fixture.node = broker.mcp_execution_node().await.unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = hive_daemon::router(broker.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let owner = broker.clone();
        let admission = fixture.admission(Scope::Machine, Some(fixture.directory.path()), 0);
        let cwd = fixture.directory.path().to_path_buf();
        let (saved, received) = tokio::sync::oneshot::channel();
        Client
            .v2()
            .connect_with(
                HttpClient::new(format!("ws://{address}")).unwrap(),
                async move |cx| {
                    cx.send_request(v2::InitializeRequest::new(
                        ProtocolVersion::V2,
                        v2::Implementation::new("mcp-worker-test", "1"),
                    ))
                    .block_task()
                    .await?;
                    let opened = cx
                        .send_request(v2::NewSessionRequest::new(cwd))
                        .block_task()
                        .await?;
                    let binding = owner
                        .bind_scoped_mcp(&opened.session_id, admission)
                        .await
                        .unwrap();
                    binding
                        .request(tool("echo", json!({})))
                        .unwrap()
                        .result()
                        .await
                        .unwrap();
                    let mut meta = opened.meta.unwrap();
                    meta.get_mut("hive").unwrap()["key"] = json!(uuid::Uuid::new_v4().to_string());
                    let _ = cx
                        .send_request(
                            v2::PromptRequest::new(
                                opened.session_id.clone(),
                                vec!["recovery-accepted-lost-ack".into()],
                            )
                            .meta(meta),
                        )
                        .block_task()
                        .await;
                    tokio::time::timeout(Duration::from_secs(3), async {
                        loop {
                            match binding.request(tool("echo", json!({}))) {
                                Err(RuntimeError::Binding) => break,
                                Ok(call) => {
                                    let _ = call.result().await;
                                }
                                Err(error) => panic!("unexpected binding failure: {error}"),
                            }
                            tokio::task::yield_now().await;
                        }
                    })
                    .await
                    .unwrap();
                    saved.send((opened.session_id, binding.id())).unwrap();
                    Ok(())
                },
            )
            .await
            .unwrap();
        let (session, binding) = received.await.unwrap();
        if persistent {
            assert!(!fixture.events().iter().any(|event| event["event"] == "eof"));
            broker.release_mcp(&session, binding);
        }
        fixture
            .observed(|events| events.iter().any(|event| event["event"] == "eof"))
            .await;
        broker.shutdown().await;
        assert!(matches!(
            broker
                .bind_scoped_mcp(
                    &session,
                    fixture.admission(Scope::Machine, Some(fixture.directory.path()), 0)
                )
                .await,
            Err(hive_daemon::mcp::AdmissionError::Runtime(
                RuntimeError::Stopping
            ))
        ));
        server.abort();
    }
}

#[cfg(unix)]
#[tokio::test]
async fn protocol_eof_or_broken_pipe_terminates_a_still_running_leader() {
    let fixture = Fixture::new();
    let lease = fixture.runtime.acquire(fixture.identity("eof", 0)).unwrap();
    let call = lease.request(McpRequest::new("ping", json!({})).unwrap());
    fixture
        .observed(|events| events.iter().any(|event| event["event"] == "spawn"))
        .await;
    let pid = fixture
        .events()
        .iter()
        .find(|event| event["event"] == "spawn")
        .unwrap()["pid"]
        .as_u64()
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(20), async {
        if let Err(error) = call.result().await {
            return error;
        }
        loop {
            match lease
                .request(McpRequest::new("ping", json!({})).unwrap())
                .result()
                .await
            {
                Ok(_) => tokio::task::yield_now().await,
                Err(error) => break error,
            }
        }
    })
    .await
    .unwrap();
    assert!(matches!(
        result,
        RuntimeError::Connection | RuntimeError::Backoff { .. }
    ));
    // Shutdown must not be the action that finally kills the disconnected leader.
    #[cfg(target_os = "linux")]
    terminated(pid).await;
    #[cfg(not(target_os = "linux"))]
    let _ = pid;
    fixture.runtime.shutdown().await;
}

#[tokio::test]
async fn closing_a_session_defers_workspace_release_until_previously_released_calls_settle() {
    released_binding_cleanup(true).await;
}

#[tokio::test]
async fn releasing_a_scoped_binding_retires_its_reservation_while_its_session_remains_open() {
    released_binding_cleanup(false).await;
}

async fn released_binding_cleanup(close_session: bool) {
    use agent_client_protocol::{
        Client,
        schema::{ProtocolVersion, v2},
    };
    use agent_client_protocol_http::HttpClient;
    let mut fixture = Fixture::new();
    let cwd = fixture.directory.path().to_path_buf();
    let config = serde_json::from_value(json!({"default_agent":"mock", "projects":{"demo":{"name":"Demo"}}, "workspaces":{"task":{"policy":"existing", "directory":cwd}}, "agents":{"mock":{"command":env!("CARGO_BIN_EXE_hive-test-agent"), "versions":["v2"], "project":"demo", "workspace":"task"}}})).unwrap();
    let broker =
        hive_daemon::Broker::persistent(config, cwd.join("state/catalog.sqlite3")).unwrap();
    fixture.node = broker.mcp_execution_node().await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = hive_daemon::router(broker.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let (ready, received) = tokio::sync::oneshot::channel();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let client = tokio::spawn(Client.v2().connect_with(
        HttpClient::new(format!("ws://{address}")).unwrap(),
        async move |cx| {
            cx.send_request(v2::InitializeRequest::new(
                ProtocolVersion::V2,
                v2::Implementation::new("mcp-workspace-test", "1"),
            ))
            .block_task()
            .await?;
            let opened = cx
                .send_request(v2::NewSessionRequest::new(cwd))
                .block_task()
                .await?;
            ready
                .send((cx, opened))
                .unwrap_or_else(|_| panic!("test receiver dropped"));
            let _ = stopped.await;
            Ok(())
        },
    ));
    let (cx, opened) = received.await.unwrap();
    let workspace = opened.meta.as_ref().unwrap()["hive"]["workspace"]["workspace"]
        .as_str()
        .unwrap()
        .to_owned();
    let binding = broker
        .bind_scoped_mcp(
            &opened.session_id,
            fixture.admission(Scope::Workspace, None, 1),
        )
        .await
        .unwrap();
    let call = binding.request(tool("hold", json!({}))).unwrap();
    fixture
        .observed(|events| events.iter().any(|event| event["params"]["name"] == "hold"))
        .await;
    broker.release_mcp(&opened.session_id, binding.id());
    assert_eq!(call.result().await, Err(RuntimeError::Cancelled));
    if close_session {
        cx.send_request(v2::CloseSessionRequest::new(opened.session_id).meta(opened.meta))
            .block_task()
            .await
            .unwrap();
    }
    let inspect = || async {
        reqwest::get(format!(
            "http://{address}/v1/workspaces/{workspace}/inspect"
        ))
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap()
    };
    assert_eq!(inspect().await["safety"]["usage"], "unknown");
    std::fs::write(format!("{}.release", fixture.log().display()), b"complete").unwrap();
    // The response alone cannot release files while a positive-idle process is still alive.
    fixture
        .observed(|events| events.iter().any(|event| event["event"] == "eof"))
        .await;
    tokio::time::timeout(Duration::from_secs(5), async {
        let expected = if close_session { "idle" } else { "active" };
        while inspect().await["safety"]["usage"] != expected {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("workspace hold was not released after terminal response");
    let _ = stop.send(());
    client.await.unwrap().unwrap();
    broker.shutdown().await;
    server.abort();
}
