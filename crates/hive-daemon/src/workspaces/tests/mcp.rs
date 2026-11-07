use super::*;
use crate::mcp::{
    Credentials, Initialization, InstanceIdentity, Lifecycle, ResolvedTransport, Scope,
    ScopeContext, SecurityContext, ServerDefinition, Transport,
};

#[tokio::test]
async fn workspace_tool_scope_is_derived_from_the_session_hold_and_rejects_foreign_owners() {
    use hive_protocol::workspace_tools::{self as tools, ToolError};
    let fixture = Fixture::new();
    let entry = fixture.create(fixture.request()).await.unwrap();
    let root = PathBuf::from(entry.workspace.location().unwrap().root.as_str());
    fixture
        .service
        .attach(entry.workspace.id().clone(), "session".into())
        .await
        .unwrap();
    let context = hive_protocol::workspace::Context {
        owner: Some(entry.workspace.intent().node.clone()),
        project: entry.workspace.intent().project.to_string(),
        workspace: entry.workspace.id().to_string(),
        effective_cwd: root.display().to_string(),
    };
    let result = fixture
        .service
        .tool_capabilities("session".into(), context.clone(), root.clone(), || Ok(()))
        .await
        .unwrap();
    assert_eq!(result.repository_backend.as_deref(), Some("git"));
    assert_eq!(result.provisioner, "git.worktree");
    let mut expected = tools::catalog(true);
    expected.extend(tools::lifecycle::catalog(true));
    assert_eq!(result.tools, expected);
    assert!(matches!(
        fixture
            .service
            .tool_capabilities(
                "other-session".into(),
                context.clone(),
                root.clone(),
                || Ok(())
            )
            .await,
        Err(ToolError::Scope)
    ));
    let mut foreign = context.clone();
    foreign.owner = Some(NodeId::new());
    // A nonexistent path would fail inspection, so the distinct Owner error proves ordering.
    assert!(matches!(
        fixture
            .service
            .tool_capabilities("session".into(), foreign, root.join("missing"), || Ok(()))
            .await,
        Err(ToolError::Owner)
    ));
    let mut changed = context.clone();
    changed.workspace = WorkspaceId::new().to_string();
    assert!(matches!(
        fixture
            .service
            .tool_capabilities("session".into(), changed, root.clone(), || Ok(()))
            .await,
        Err(ToolError::Scope)
    ));
    fixture.service.release("session".into()).await.unwrap();
    assert!(matches!(
        fixture
            .service
            .tool_capabilities("session".into(), context, root, || Ok(()))
            .await,
        Err(ToolError::Scope)
    ));
}

#[tokio::test]
async fn built_in_calls_retain_settlement_after_close_caller_drop_recovery_and_shutdown() {
    use crate::mcp::{PendingTarget, StartupOwnership, workspace::Provider};
    // All cases fence native dispatch, so they can share one provisioned checkout.
    let fixture = Fixture::new();
    let entry = fixture.create(fixture.request()).await.unwrap();
    for (action, operation) in ["close", "drop", "recovery", "shutdown"]
        .into_iter()
        .flat_map(|action| ["read", "commit", "create"].map(|operation| (action, operation)))
    {
        let project = entry.workspace.intent().project.clone();
        let mut config = crate::Config::default();
        config
            .projects
            .insert(project.clone(), fixture.definition.clone());
        // Exercise the real workspace store without starting an unrelated marker publisher
        // or reopening SQLite per case. The cancellation registry must remain fresh per case.
        let mut broker = crate::Broker::new(config).unwrap();
        broker.store = Some(fixture.service.store.clone());
        let session = hive_protocol::v2::SessionId::new("built-in-session");
        fixture
            .service
            .attach(entry.workspace.id().clone(), session.to_string())
            .await
            .unwrap();
        let cwd = PathBuf::from(entry.workspace.location().unwrap().root.as_str());
        std::fs::write(cwd.join("content"), "pending commit\n").unwrap();
        let mut state = crate::control_tests::detached();
        state.worktrunk_local = true;
        state.workspace_context = Some(hive_protocol::workspace::Context {
            owner: Some(entry.workspace.intent().node.clone()),
            project: project.to_string(),
            workspace: entry.workspace.id().to_string(),
            effective_cwd: cwd.display().to_string(),
        });
        let control = Arc::new(std::sync::Mutex::new(state));
        let mut startup = broker
            .begin_mcp_startup(
                session.to_string(),
                PendingTarget {
                    control: control.clone(),
                    cwd: hive_protocol::v2::AbsolutePath::new(cwd),
                },
                StartupOwnership::Recovery,
            )
            .unwrap();
        let mut provider = Provider::prepare(&broker, &session).unwrap().unwrap();
        let (entered, ready) = tokio::sync::oneshot::channel();
        let (resume, waiting) = std::sync::mpsc::channel();
        let entered = std::sync::Mutex::new(Some(entered));
        let waiting = std::sync::Mutex::new(waiting);
        provider.before_inspect = Some(Arc::new(move || {
            entered.lock().unwrap().take().unwrap().send(()).unwrap();
            waiting.lock().unwrap().recv().unwrap();
        }));
        let old = provider.clone();
        let call = tokio::spawn(async move {
            let request = if operation == "create" {
                hive_protocol::workspace_tools::Request::Workspace(
                    hive_protocol::workspace_tools::lifecycle::Request::Create {
                        operation: OperationId::new(),
                        base_ref: None,
                        branch: None,
                        revision: None,
                    },
                )
            } else {
                operation
                    .eq("commit")
                    .then(|| hive_core::workspace::backend::vcs::Request::Commit {
                        message: "must not dispatch".into(),
                        inclusion:
                            hive_core::workspace::backend::vcs::Inclusion::TrackedWorkingCopy,
                        untracked: vec![],
                        ignored: vec![],
                        expected_state: None,
                    })
                    .into()
            };
            provider.execute(request, None).await
        });
        ready.await.unwrap();
        assert_eq!(control.lock().unwrap().pending_builtin_operations, 1);
        match action {
            "close" => control.lock().unwrap().closing = true,
            "drop" => call.abort(),
            "recovery" => {
                broker
                    .mcp_sessions
                    .pending
                    .lock()
                    .unwrap()
                    .get_mut(&session.to_string())
                    .unwrap()
                    .control = Arc::new(std::sync::Mutex::new(crate::control_tests::detached()));
                broker.mcp_sessions.cancel(&session.to_string());
            }
            "shutdown" => broker.registry.lock().unwrap().stopping = true,
            _ => unreachable!(),
        }
        let evidence = if action == "shutdown" {
            broker.mcp_sessions.clear();
            None
        } else {
            Some(broker.mcp_sessions.finish(&session.to_string()))
        };
        if let Some(pending) = &evidence {
            assert_eq!(pending.len(), 1);
            assert!(
                !*pending[0].borrow(),
                "cancellation is not resource settlement"
            );
        }
        let sessions = broker.mcp_sessions.clone();
        assert_eq!(control.lock().unwrap().pending_builtin_operations, 1);
        let mut waiter = Box::pin(async move {
            match evidence {
                Some(pending) => crate::mcp::settled(pending).await,
                None => sessions.drain_cleanup().await,
            }
        });
        // Poll while the resource job is blocked: an unpolled spawned task proves nothing.
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(waiter.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        resume.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .unwrap();
        let _ = call.await;
        assert_eq!(control.lock().unwrap().pending_builtin_operations, 0);
        if operation == "commit" {
            let root = Path::new(entry.workspace.location().unwrap().root.as_str());
            assert_eq!(
                git(root, &["log", "-1", "--format=%s"], "test")
                    .unwrap()
                    .trim(),
                "initial"
            );
        }
        if operation == "create" {
            assert!(
                fixture
                    .service
                    .list()
                    .await
                    .unwrap()
                    .iter()
                    .all(|workspace| workspace.workspace.id() == entry.workspace.id()
                        || workspace.workspace.state() == WorkspaceState::Failed)
            );
        }
        assert!(old.capabilities(None).await.is_err());
        startup.promoted = true;
        drop(startup);
        // Lifecycle completion still does not remove a workspace or implicitly release its hold.
        assert!(
            fixture
                .service
                .attached(session.to_string())
                .await
                .unwrap()
                .is_some()
        );
        fixture.service.release(session.to_string()).await.unwrap();
        broker.shutdown().await;
    }
}

#[tokio::test]
async fn machine_reservations_protect_extra_resources_without_a_primary_session_and_survive_restart()
 {
    let fixture = Fixture::new();
    let first = fixture.create(fixture.request()).await.unwrap();
    let second = fixture.create(fixture.request()).await.unwrap();
    let first_id = first.workspace.id().clone();
    let second_id = second.workspace.id().clone();
    let executable = Path::new(first.workspace.location().unwrap().root.as_str()).join("server");
    std::fs::write(&executable, "fixture").unwrap();
    let cwd = PathBuf::from(second.workspace.location().unwrap().root.as_str());
    let id = uuid::Uuid::new_v4();
    let root = fixture._directory.path().canonicalize().unwrap();
    let neutral = fixture.seed.clone();
    fixture
        .service
        .run(move |rt| {
            let identity = InstanceIdentity::shared(
                "fixture".into(),
                ServerDefinition::new(
                    Transport::Stdio {
                        command: "server".into(),
                        args: vec![],
                        env: BTreeMap::new(),
                    },
                    Scope::Machine,
                    crate::mcp::Ownership::Managed,
                    Lifecycle::OnDemand {
                        idle_timeout_seconds: 60,
                    },
                )
                .unwrap(),
                ScopeContext {
                    node: rt.state.node.clone(),
                    project: None,
                    workspace: None,
                },
                ResolvedTransport::Stdio {
                    executable: rt.path(&executable)?,
                    args: vec![],
                    env: BTreeMap::new(),
                    cwd: rt.path(&cwd)?,
                },
                Initialization {
                    protocol_version: "2025-11-25".into(),
                    client_info: serde_json::json!({"name":"test","version":"1"}),
                    capabilities: serde_json::json!({}),
                    options: serde_json::json!({}),
                    roots: vec![rt.path(&root)?],
                },
                SecurityContext {
                    principal: "test".into(),
                    partition: "test".into(),
                    policy_generation: "1".into(),
                    credentials: Credentials::Complete {
                        external_generations: BTreeMap::new(),
                    },
                },
            )
            .unwrap();
            let resources = rt.mcp_dependencies(&identity).unwrap();
            assert_eq!(
                resources,
                BTreeSet::from([first_id.clone(), second_id.clone()])
            );
            let mut isolated = identity.clone();
            isolated.initialization.roots.clear();
            let ResolvedTransport::Stdio { cwd, .. } = &mut isolated.transport else {
                unreachable!()
            };
            let managed_cwd = cwd.clone();
            *cwd = rt.path(&neutral)?;
            assert_eq!(
                rt.mcp_dependencies(&isolated).unwrap(),
                BTreeSet::from([first_id.clone()]),
                "executable alone must retain its workspace"
            );
            let ResolvedTransport::Stdio {
                executable, cwd, ..
            } = &mut isolated.transport
            else {
                unreachable!()
            };
            *executable = rt.path(&std::env::current_exe().unwrap())?;
            *cwd = managed_cwd;
            assert_eq!(
                rt.mcp_dependencies(&isolated).unwrap(),
                BTreeSet::from([second_id]),
                "cwd alone must retain its workspace"
            );
            let ResolvedTransport::Stdio { cwd, .. } = &mut isolated.transport else {
                unreachable!()
            };
            *cwd = rt.path(&neutral)?;
            isolated.initialization.roots = vec![rt.path(executable_path_parent(&identity))?];
            assert_eq!(
                rt.mcp_dependencies(&isolated).unwrap(),
                BTreeSet::from([first_id]),
                "an explicit root alone must retain its workspace"
            );
            assert!(rt.state.uses.is_empty());
            rt.state.mcp_reservations.insert(
                id.to_string(),
                mcp::Reservation {
                    session: "workspace-less".into(),
                    workspaces: resources,
                },
            );
            rt.save()
        })
        .await
        .unwrap();
    let restarted = Service::new(fixture.service.store.clone());
    for entry in [&first, &second] {
        assert!(matches!(
            restarted
                .inspect(entry.workspace.id().clone())
                .await
                .unwrap(),
            Inspection::Present {
                safety: SafetyEvidence {
                    usage: ResourceUse::Unknown,
                    ..
                },
                ..
            }
        ));
        assert!(
            restarted
                .remove(entry.workspace.id().clone(), OperationId::new(), true)
                .await
                .is_err()
        );
    }
    let report = serde_json::to_value(restarted.recovery(BTreeSet::new()).await.unwrap()).unwrap();
    assert_eq!(report["mcp_reservations"][0]["id"], id.to_string());
    assert!(
        !report["workspaces"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["classifications"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value == "unreferenced"))
    );
    // This explicit trusted recovery call requires external evidence, never absence of a session.
    let fresh = crate::Broker::persistent(
        Default::default(),
        fixture._directory.path().join("state/catalog.sqlite3"),
    )
    .unwrap();
    fresh
        .release_mcp_reservation_after_verified_termination(id)
        .await
        .unwrap();
    fresh.mcp_execution_node().await.unwrap();
    assert!(matches!(
        fresh
            .release_mcp_reservation_after_verified_termination(id)
            .await,
        Err(crate::mcp::AdmissionError::Runtime(
            crate::mcp::runtime::RuntimeError::Binding
        ))
    ));
    fresh.shutdown().await;
    for entry in [&first, &second] {
        assert!(matches!(
            restarted
                .inspect(entry.workspace.id().clone())
                .await
                .unwrap(),
            Inspection::Present {
                safety: SafetyEvidence {
                    usage: ResourceUse::Idle,
                    ..
                },
                ..
            }
        ));
    }
}

#[cfg(unix)]
#[tokio::test]
async fn a_commit_already_dispatched_retains_its_known_result_and_settlement_after_close() {
    use crate::mcp::{PendingTarget, StartupOwnership, workspace::Provider};
    use hive_core::workspace::backend::vcs::{Inclusion, Request};
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    let entry = fixture.create(fixture.request()).await.unwrap();
    let root = PathBuf::from(entry.workspace.location().unwrap().root.as_str());
    std::fs::write(
        root.join("content"),
        "recorded despite cancelled delivery\n",
    )
    .unwrap();
    let hooks = fixture._directory.path().join("blocking-hooks");
    std::fs::create_dir(&hooks).unwrap();
    let hook = hooks.join("pre-commit");
    std::fs::write(
        &hook,
        "#!/bin/sh\ntouch hook-entered\nwhile [ ! -e hook-release ]; do sleep 0.01; done\n",
    )
    .unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o700)).unwrap();
    git(
        &root,
        &["config", "core.hooksPath", hooks.to_str().unwrap()],
        "test",
    )
    .unwrap();
    let project = entry.workspace.intent().project.clone();
    let mut config = crate::Config::default();
    config
        .projects
        .insert(project.clone(), fixture.definition.clone());
    let broker = crate::Broker::persistent(
        config,
        fixture
            .service
            .store
            .state_directory()
            .join("catalog.sqlite3"),
    )
    .unwrap();
    let session = hive_protocol::v2::SessionId::new("dispatched-commit-session");
    fixture
        .service
        .attach(entry.workspace.id().clone(), session.to_string())
        .await
        .unwrap();
    let mut state = crate::control_tests::detached();
    state.worktrunk_local = true;
    state.workspace_context = Some(hive_protocol::workspace::Context {
        owner: Some(entry.workspace.intent().node.clone()),
        project: project.to_string(),
        workspace: entry.workspace.id().to_string(),
        effective_cwd: root.display().to_string(),
    });
    let control = Arc::new(std::sync::Mutex::new(state));
    let mut startup = broker
        .begin_mcp_startup(
            session.to_string(),
            PendingTarget {
                control: control.clone(),
                cwd: hive_protocol::v2::AbsolutePath::new(root.clone()),
            },
            StartupOwnership::Recovery,
        )
        .unwrap();
    let provider = Provider::prepare(&broker, &session).unwrap().unwrap();
    let call = tokio::spawn(async move {
        provider
            .execute(
                Some(Request::Commit {
                    message: "retained native success".into(),
                    inclusion: Inclusion::TrackedWorkingCopy,
                    untracked: vec![],
                    ignored: vec![],
                    expected_state: None,
                }),
                None,
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(10), async {
        while !root.join("hook-entered").exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    control.lock().unwrap().closing = true;
    broker.mcp_sessions.cancel(&session.to_string());
    let pending = broker.mcp_sessions.finish(&session.to_string());
    assert_eq!(pending.len(), 1);
    assert!(
        !*pending[0].borrow(),
        "a running native hook retains the resource hold"
    );
    std::fs::write(root.join("hook-release"), "").unwrap();
    let result = tokio::time::timeout(Duration::from_secs(10), call)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(result["outcome"], "recorded");
    assert_eq!(
        result["recorded"]["identity"]["commitId"],
        git(&root, &["rev-parse", "HEAD"], "test").unwrap().trim()
    );
    crate::mcp::settled(pending).await;
    startup.promoted = true;
    fixture.service.release(session.to_string()).await.unwrap();
    broker.shutdown().await;
}

fn executable_path_parent(identity: &InstanceIdentity) -> &Path {
    let ResolvedTransport::Stdio { executable, .. } = &identity.transport else {
        unreachable!()
    };
    Path::new(executable.as_str()).parent().unwrap()
}

async fn admission_fixture() -> (
    Fixture,
    crate::Broker,
    hive_protocol::v2::SessionId,
    crate::mcp::StdioAdmission,
) {
    let fixture = Fixture::new();
    let workspace = fixture.create(fixture.request()).await.unwrap();
    let broker = crate::Broker::persistent(
        Default::default(),
        fixture
            .service
            .store
            .state_directory()
            .join("catalog.sqlite3"),
    )
    .unwrap();
    let session = hive_protocol::v2::SessionId::new("mcp-session");
    let worker = tokio::spawn(std::future::pending::<()>());
    broker.sessions.lock().unwrap().insert(
        session.to_string(),
        crate::Entry {
            cwd: hive_protocol::v2::AbsolutePath::new(fixture.seed.clone()),
            profile: "test".into(),
            abort: worker.abort_handle(),
            commands: tokio::sync::mpsc::unbounded_channel().0,
            control: Arc::new(std::sync::Mutex::new(crate::control_tests::detached())),
        },
    );
    let node = broker.mcp_execution_node().await.unwrap();
    let path = |path: &Path| {
        NodePath::new(
            node.clone(),
            if cfg!(windows) {
                PathSyntax::Windows
            } else {
                PathSyntax::Posix
            },
            path.to_str().unwrap().into(),
        )
        .unwrap()
    };
    let admission = crate::mcp::StdioAdmission {
        definition_id: "extra-resource".into(),
        definition: ServerDefinition::new(
            Transport::Stdio {
                command: "test-server".into(),
                // The test executable exits with a controlled argument error: a failed process still
                // leaves a restart-capable binding that must retain its resource reservation.
                args: vec!["--hive-invalid-mcp-fixture-argument".into()],
                env: BTreeMap::new(),
            },
            Scope::Machine,
            crate::mcp::Ownership::Managed,
            Lifecycle::OnDemand {
                idle_timeout_seconds: 0,
            },
        )
        .unwrap(),
        launch: crate::mcp::StdioLaunch {
            executable: path(&std::env::current_exe().unwrap()),
            env: BTreeMap::new(),
            cwd: Some(workspace.workspace.location().unwrap().root.clone()),
        },
        initialization: Initialization {
            protocol_version: "2025-11-25".into(),
            client_info: serde_json::json!({"name":"test", "version":"1"}),
            capabilities: serde_json::json!({}),
            options: serde_json::json!({}),
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
        sharing: crate::mcp::Sharing::Shared,
    };
    (fixture, broker, session, admission)
}

async fn reservations(service: &Service, expected: usize) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let count = service
                .run(|rt| Ok(rt.state.mcp_reservations.len()))
                .await
                .unwrap();
            if count == expected {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("MCP reservations did not reach the expected state");
}

async fn deliveries(
    broker: &crate::Broker,
    session: &hive_protocol::v2::SessionId,
    expected: usize,
) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while broker.mcp_sessions.pending_deliveries(&session.to_string()) != expected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("admission delivery did not reach its barrier");
}

#[tokio::test]
async fn scoped_machine_admission_upgrades_an_unprotected_binding_and_releases_extra_resources_while_open()
 {
    let (fixture, broker, session, admission) = admission_fixture().await;
    let identity = admission
        .clone()
        .identity(
            ScopeContext {
                node: broker.mcp_execution_node().await.unwrap(),
                project: None,
                workspace: None,
            },
            None,
        )
        .unwrap();
    let low_level = broker.bind_mcp(&session, identity).await.unwrap();
    reservations(&fixture.service, 0).await;
    let scoped = broker.bind_scoped_mcp(&session, admission).await.unwrap();
    assert_eq!(low_level.id(), scoped.id());
    reservations(&fixture.service, 1).await;
    fixture
        .service
        .run(|rt| {
            assert!(
                rt.state.uses.is_empty(),
                "machine admission must not invent a primary attachment"
            );
            assert!(
                rt.state
                    .workspaces
                    .values()
                    .all(|entry| rt.has_use(entry.workspace.id()))
            );
            Ok(())
        })
        .await
        .unwrap();
    broker.release_mcp(&session, scoped.id());
    reservations(&fixture.service, 0).await;
    assert!(
        broker
            .sessions
            .lock()
            .unwrap()
            .contains_key(&session.to_string())
    );
    broker.shutdown().await;
}

#[tokio::test]
async fn cancelling_an_unread_delivery_preserves_accepted_and_pending_duplicate_deliveries() {
    use std::future::Future;
    let (fixture, broker, session, admission) = admission_fixture().await;
    for accept_before_cancel in [true, false] {
        let mut unread = Box::pin(broker.bind_scoped_mcp(&session, admission.clone()));
        std::future::poll_fn(|cx| {
            assert!(unread.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        reservations(&fixture.service, 1).await;
        // The resource job committed; the coordinator gate also proves delivery has been sent.
        drop(broker.mcp_sessions.admission.lock().await);
        let mut duplicate = Box::pin(broker.bind_scoped_mcp(&session, admission.clone()));
        let accepted = if accept_before_cancel {
            Some(duplicate.as_mut().await.unwrap())
        } else {
            std::future::poll_fn(|cx| {
                assert!(duplicate.as_mut().poll(cx).is_pending());
                std::task::Poll::Ready(())
            })
            .await;
            deliveries(&broker, &session, 2).await;
            None
        };
        drop(unread);
        let survivor = if accept_before_cancel {
            accepted.unwrap()
        } else {
            duplicate.await.unwrap()
        };
        assert!(
            survivor
                .request(
                    crate::mcp::runtime::McpRequest::new("ping", serde_json::json!({})).unwrap()
                )
                .is_ok(),
            "cancelling one delivery must not revoke its duplicate"
        );
        reservations(&fixture.service, 1).await;
        broker.release_mcp(&session, survivor.id());
        reservations(&fixture.service, 0).await;
    }
    let mut first = Box::pin(broker.bind_scoped_mcp(&session, admission.clone()));
    std::future::poll_fn(|cx| {
        assert!(first.as_mut().poll(cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    deliveries(&broker, &session, 1).await;
    let mut second = Box::pin(broker.bind_scoped_mcp(&session, admission));
    std::future::poll_fn(|cx| {
        assert!(second.as_mut().poll(cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    deliveries(&broker, &session, 2).await;
    drop(first);
    drop(second);
    reservations(&fixture.service, 0).await;
    broker.shutdown().await;
}

#[tokio::test]
async fn cancellation_during_a_blocked_resource_job_rolls_back_without_leaking_a_reservation() {
    use std::future::Future;
    let (fixture, broker, session, admission) = admission_fixture().await;
    let service = fixture.service.clone();
    let (entered, ready) = tokio::sync::oneshot::channel();
    let (resume, waiting) = std::sync::mpsc::channel();
    let blocker = tokio::spawn(async move {
        service
            .run(move |_| {
                entered.send(()).unwrap();
                waiting.recv().unwrap();
                Ok(())
            })
            .await
            .unwrap();
    });
    ready.await.unwrap();
    let mut pending = Box::pin(broker.bind_scoped_mcp(&session, admission));
    std::future::poll_fn(|cx| {
        assert!(pending.as_mut().poll(cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if broker.mcp_sessions.admission.try_lock().is_err() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    drop(pending);
    resume.send(()).unwrap();
    blocker.await.unwrap();
    drop(broker.mcp_sessions.admission.lock().await);
    reservations(&fixture.service, 0).await;
    broker.shutdown().await;
}

#[tokio::test]
async fn scoped_evidence_rejects_stale_context_nested_checkouts_and_duplicate_roots() {
    let fixture = Fixture::new();
    let entry = fixture.create(fixture.request()).await.unwrap();
    fixture
        .service
        .attach(entry.workspace.id().clone(), "session".into())
        .await
        .unwrap();
    let root = PathBuf::from(entry.workspace.location().unwrap().root.as_str());
    let context = hive_protocol::workspace::Context {
        owner: Some(entry.workspace.intent().node.clone()),
        project: entry.workspace.intent().project.to_string(),
        workspace: entry.workspace.id().to_string(),
        effective_cwd: root.to_str().unwrap().into(),
    };
    let nested = root.join("nested");
    std::fs::create_dir(&nested).unwrap();
    git(&nested, &["init"], "test").unwrap();
    fixture
        .service
        .run(move |rt| {
            assert!(rt.mcp_scope("session", Some(&context), &root, true).is_ok());
            assert!(
                rt.mcp_scope("session", Some(&context), &nested, true)
                    .is_err()
            );
            let mut wrong = context.clone();
            wrong.project = "missing".into();
            assert!(rt.mcp_scope("session", Some(&wrong), &root, true).is_err());
            let mut duplicate = rt.state.workspaces[&context.workspace].clone();
            // A second external registration can point at the same concrete root.
            let request = CreateRequest {
                operation: OperationId::new(),
                project: duplicate.workspace.intent().project.clone(),
                policy: WorkspaceTemplate::Existing {
                    directory: Some(root.clone()),
                },
                base_ref: None,
                branch: None,
                revision: None,
            };
            duplicate = rt.create(
                request,
                serde_json::from_value(serde_json::json!({"name":"Alias"})).unwrap(),
                WorktrunkConfig {
                    enabled: Some(false),
                },
            )?;
            assert_ne!(duplicate.workspace.id().as_str(), context.workspace);
            assert!(
                rt.mcp_scope("session", Some(&context), &root, true)
                    .is_err()
            );
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn version_one_resources_upgrade_and_malformed_reservations_fail_closed() {
    let fixture = Fixture::new();
    let entry = fixture.create(fixture.request()).await.unwrap();
    let mut snapshot: serde_json::Value = fixture.service.store.resource("node").unwrap().unwrap();
    snapshot["version"] = serde_json::json!(1);
    snapshot.as_object_mut().unwrap().remove("mcp_reservations");
    let operations = snapshot["operations"].clone();
    fixture
        .service
        .store
        .save_resource("node", &snapshot)
        .unwrap();
    fixture.service.list().await.unwrap();
    let mut upgraded: serde_json::Value = fixture.service.store.resource("node").unwrap().unwrap();
    assert_eq!(upgraded["version"], 2);
    assert_eq!(upgraded["operations"], operations);
    upgraded["mcp_reservations"] = serde_json::json!({"invalid-id":{"session":"session", "workspaces":[entry.workspace.id()]}});
    fixture
        .service
        .store
        .save_resource("node", &upgraded)
        .unwrap();
    assert!(fixture.service.list().await.is_err());
    upgraded["mcp_reservations"] = serde_json::json!({});
    upgraded["version"] = serde_json::json!(3);
    fixture
        .service
        .store
        .save_resource("node", &upgraded)
        .unwrap();
    assert!(fixture.service.list().await.is_err());
}
