use super::*;
use crate::mcp::{
    Credentials, Initialization, InstanceIdentity, Lifecycle, ResolvedTransport, Scope,
    ScopeContext, SecurityContext, ServerDefinition, Transport,
};

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
