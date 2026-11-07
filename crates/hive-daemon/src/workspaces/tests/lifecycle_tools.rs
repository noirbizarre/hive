use super::*;
use hive_protocol::workspace_tools::{self as tools, ToolError, lifecycle::Request};
use serde_json::{Value, json};

fn context(entry: &Entry) -> hive_protocol::workspace::Context {
    hive_protocol::workspace::Context {
        owner: Some(entry.workspace.intent().node.clone()),
        project: entry.workspace.intent().project.to_string(),
        workspace: entry.workspace.id().to_string(),
        effective_cwd: entry.workspace.location().unwrap().root.as_str().into(),
    }
}

async fn call(service: &Service, parent: &Entry, request: Request) -> Result<Value, ToolError> {
    service
        .tool_execute(
            "agent".into(),
            context(parent),
            parent.workspace.location().unwrap().root.as_str().into(),
            tools::Request::Workspace(request),
            || Ok(()),
        )
        .await
}

fn create(operation: OperationId) -> Request {
    Request::Create {
        operation,
        base_ref: None,
        branch: None,
        revision: None,
    }
}

async fn attach(service: &Service, parent: &Entry) {
    service
        .attach(parent.workspace.id().clone(), "agent".into())
        .await
        .unwrap();
}

#[tokio::test]
async fn siblings_inherit_pinned_intent_without_reusing_the_parent_branch_or_session_hold() {
    let fixture = Fixture::new();
    let mut request = fixture.request();
    request.branch = Some("parent-task".to_owned().try_into().unwrap());
    let parent = fixture.create(request).await.unwrap();
    attach(&fixture.service, &parent).await;
    let operation = OperationId::new();
    let result = call(&fixture.service, &parent, create(operation.clone()))
        .await
        .unwrap();
    let child = WorkspaceId::try_from(result["workspace"].as_str().unwrap().to_owned()).unwrap();
    let entry = fixture
        .service
        .list()
        .await
        .unwrap()
        .into_iter()
        .find(|entry| entry.workspace.id() == &child)
        .unwrap();
    assert_eq!(entry.selection, parent.selection);
    assert_eq!(entry.repository, parent.repository);
    assert_ne!(
        entry.workspace.location().unwrap().branch,
        parent.workspace.location().unwrap().branch
    );
    assert_eq!(
        entry
            .workspace
            .location()
            .unwrap()
            .branch
            .as_ref()
            .map(GitRef::as_str),
        Some(format!("hive/{child}").as_str())
    );
    assert_eq!(
        fixture
            .service
            .attached("agent".into())
            .await
            .unwrap()
            .unwrap()
            .workspace
            .id(),
        parent.workspace.id()
    );
    // No configuration is supplied on replay: durable intent is the only selection source.
    let restarted = Service::new(fixture.service.store.clone());
    assert_eq!(
        call(&restarted, &parent, create(operation.clone()))
            .await
            .unwrap(),
        result
    );
    let changed = Request::Create {
        operation: operation.clone(),
        base_ref: Some("HEAD".to_owned().try_into().unwrap()),
        branch: None,
        revision: None,
    };
    assert!(matches!(
        call(&restarted, &parent, changed).await,
        Err(ToolError::Lifecycle {
            code: "hive::workspace::retry_conflict"
        })
    ));
    let first = call(
        &restarted,
        &parent,
        Request::List {
            limit: 1,
            cursor: None,
        },
    )
    .await
    .unwrap();
    let cursor: WorkspaceId = serde_json::from_value(first["nextCursor"].clone()).unwrap();
    let second = call(
        &restarted,
        &parent,
        Request::List {
            limit: 1,
            cursor: Some(cursor),
        },
    )
    .await
    .unwrap();
    assert_ne!(
        first["workspaces"][0]["workspace"],
        second["workspaces"][0]["workspace"]
    );
    assert!(second["nextCursor"].is_null());
    let cleanup = OperationId::new();
    call(
        &restarted,
        &parent,
        Request::Remove {
            workspace: child.clone(),
            operation: cleanup.clone(),
        },
    )
    .await
    .unwrap();
    call(
        &restarted,
        &parent,
        Request::Remove {
            workspace: child,
            operation: cleanup.clone(),
        },
    )
    .await
    .unwrap();
    let status = call(
        &restarted,
        &parent,
        Request::Operation { operation: cleanup },
    )
    .await
    .unwrap();
    assert_eq!(status["state"], "removed");
    assert_eq!(status["persistedSnapshot"], true);
    restarted.release("agent".into()).await.unwrap();
    assert!(Path::new(parent.workspace.location().unwrap().root.as_str()).exists());
}

#[tokio::test]
async fn project_selectors_do_not_grant_access_to_foreign_workspaces_or_operations() {
    let fixture = Fixture::new();
    let parent = fixture.create(fixture.request()).await.unwrap();
    let mut foreign = fixture.request();
    foreign.project = "other-project".to_owned().try_into().unwrap();
    let operation = foreign.operation.clone();
    let foreign = fixture.create(foreign).await.unwrap();
    attach(&fixture.service, &parent).await;
    for request in [
        Request::Info {
            workspace: Some(foreign.workspace.id().clone()),
        },
        Request::Remove {
            workspace: foreign.workspace.id().clone(),
            operation: OperationId::new(),
        },
        Request::Operation {
            operation: operation.clone(),
        },
        create(operation),
        Request::List {
            limit: 1,
            cursor: Some(foreign.workspace.id().clone()),
        },
    ] {
        assert_eq!(
            call(&fixture.service, &parent, request).await.unwrap_err(),
            ToolError::Scope
        );
    }
    let list = call(
        &fixture.service,
        &parent,
        Request::List {
            limit: 100,
            cursor: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(list["workspaces"].as_array().unwrap().len(), 1);
    let mut foreign_context = context(&parent);
    foreign_context.owner = Some(NodeId::new());
    assert_eq!(
        fixture
            .service
            .tool_execute(
                "agent".into(),
                foreign_context,
                PathBuf::from("/missing"),
                tools::Request::Workspace(Request::Current),
                || Ok(())
            )
            .await
            .unwrap_err(),
        ToolError::Owner
    );
}

#[tokio::test]
async fn preserve_only_agent_removal_refuses_dirty_active_external_and_unknown_resources() {
    let fixture = Fixture::new();
    let parent = fixture.create(fixture.request()).await.unwrap();
    let child = fixture.create(fixture.request()).await.unwrap();
    attach(&fixture.service, &parent).await;
    let root = Path::new(child.workspace.location().unwrap().root.as_str());
    let remove = |entry: &Entry| Request::Remove {
        workspace: entry.workspace.id().clone(),
        operation: OperationId::new(),
    };
    assert_eq!(
        call(&fixture.service, &parent, remove(&parent))
            .await
            .unwrap_err(),
        ToolError::Cleanup(hive_core::workspace::CleanupRefusal::UnknownUse)
    );
    for name in ["content", "untracked"] {
        std::fs::write(root.join(name), "do not delete\n").unwrap();
        assert_eq!(
            call(&fixture.service, &parent, remove(&child))
                .await
                .unwrap_err(),
            ToolError::Cleanup(hive_core::workspace::CleanupRefusal::Dirty)
        );
        assert!(root.join(name).exists());
        if name == "content" {
            std::fs::write(root.join(name), "original\n").unwrap();
        } else {
            std::fs::remove_file(root.join(name)).unwrap();
        }
    }
    let excludes = fixture._directory.path().join("tool-ignored");
    std::fs::write(&excludes, "ignored-only\n").unwrap();
    git(
        root,
        &["config", "core.excludesFile", excludes.to_str().unwrap()],
        "test",
    )
    .unwrap();
    std::fs::write(root.join("ignored-only"), "important ignored data\n").unwrap();
    assert_eq!(
        call(&fixture.service, &parent, remove(&child))
            .await
            .unwrap_err(),
        ToolError::Cleanup(hive_core::workspace::CleanupRefusal::Dirty)
    );
    std::fs::remove_file(root.join("ignored-only")).unwrap();
    fixture
        .service
        .attach(child.workspace.id().clone(), "other-agent".into())
        .await
        .unwrap();
    assert_eq!(
        call(&fixture.service, &parent, remove(&child))
            .await
            .unwrap_err(),
        ToolError::Cleanup(hive_core::workspace::CleanupRefusal::UnknownUse)
    );
    fixture.service.release("other-agent".into()).await.unwrap();
    let id = child.workspace.id().clone();
    let reservation = uuid::Uuid::new_v4();
    fixture
        .service
        .run(move |rt| {
            rt.state.mcp_reservations.insert(
                reservation.to_string(),
                crate::workspaces::mcp::Reservation {
                    session: "lost-machine".into(),
                    workspaces: BTreeSet::from([id]),
                },
            );
            rt.save()
        })
        .await
        .unwrap();
    assert_eq!(
        call(&fixture.service, &parent, remove(&child))
            .await
            .unwrap_err(),
        ToolError::Cleanup(hive_core::workspace::CleanupRefusal::UnknownUse)
    );
    let mut existing = fixture.request();
    existing.policy = WorkspaceTemplate::Existing {
        directory: Some(fixture.seed.clone()),
    };
    existing.base_ref = None;
    let external = fixture.create(existing).await.unwrap();
    assert_eq!(
        call(&fixture.service, &parent, remove(&external))
            .await
            .unwrap_err(),
        ToolError::Cleanup(hive_core::workspace::CleanupRefusal::ExternalOwnership)
    );
    assert!(root.exists());
}

#[tokio::test]
async fn operation_snapshots_remain_readable_while_the_resource_gate_is_held() {
    let fixture = Fixture::new();
    let request = fixture.request();
    let operation = request.operation.clone();
    let parent = fixture.create(request).await.unwrap();
    attach(&fixture.service, &parent).await;
    let (entered, ready) = tokio::sync::oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    let service = fixture.service.clone();
    let blocked = tokio::spawn(async move {
        service
            .run(move |_rt| {
                entered.send(()).unwrap();
                wait.recv().unwrap();
                Ok(())
            })
            .await
    });
    ready.await.unwrap();
    let status = tokio::time::timeout(
        Duration::from_secs(2),
        call(&fixture.service, &parent, Request::Operation { operation }),
    )
    .await;
    // Always release the worker, including when the assertion reports a status deadlock.
    release.send(()).unwrap();
    blocked.await.unwrap().unwrap();
    let status = status
        .expect("status must not wait for resources.lock")
        .unwrap();
    assert_eq!(status["persistedSnapshot"], true);
    for private in [
        "path",
        "request",
        "prepared",
        "sessions",
        "selection",
        "worktrunkConfig",
        "inspection",
    ] {
        assert!(status.get(private).is_none(), "{private}");
    }
}

#[tokio::test]
async fn revoked_authority_before_checkout_dispatch_keeps_the_original_retry_identity() {
    let fixture = Fixture::new();
    let parent = fixture.create(fixture.request()).await.unwrap();
    attach(&fixture.service, &parent).await;
    let operation = OperationId::new();
    let calls = std::sync::atomic::AtomicUsize::new(0);
    let result = fixture
        .service
        .tool_execute(
            "agent".into(),
            context(&parent),
            parent.workspace.location().unwrap().root.as_str().into(),
            tools::Request::Workspace(create(operation.clone())),
            move || {
                if calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= 2 {
                    Err(ToolError::Session)
                } else {
                    Ok(())
                }
            },
        )
        .await;
    assert!(matches!(
        result,
        Err(ToolError::Lifecycle {
            code: "hive::workspace::uncertain_operation"
        })
    ));
    let status = call(
        &fixture.service,
        &parent,
        Request::Operation {
            operation: operation.clone(),
        },
    )
    .await
    .unwrap();
    assert_eq!(status["state"], "failed");
    assert_eq!(status["effectsStarted"], false);
    let replayed = call(&fixture.service, &parent, create(operation.clone()))
        .await
        .unwrap();
    assert_eq!(replayed["workspace"], status["workspace"]);
    assert_eq!(replayed["state"], "ready");
    let request: CleanupRequest = CleanupRequest {
        operation: OperationId::new(),
        target: ResourceTarget {
            workspace: parent.workspace.id().clone(),
            node: parent.workspace.intent().node.clone(),
            backend: parent.workspace.backend().unwrap().clone(),
            ownership: Ownership::HiveManaged,
            location: parent.workspace.location().cloned(),
        },
        mode: CleanupMode::DiscardChanges,
    };
    let cleanup = request.operation.clone();
    let selection = parent.selection.clone();
    fixture
        .service
        .run(move |rt| {
            rt.state.operations.insert(
                request.operation.to_string(),
                Operation {
                    request: json!(request),
                    workspace: request.target.workspace.clone(),
                    commit: None,
                    path: None,
                    effects_started: false,
                    selection,
                    kind: Some(OperationKind::Cleanup),
                    prepared: None,
                },
            );
            rt.save()
        })
        .await
        .unwrap();
    assert!(matches!(
        call(
            &fixture.service,
            &parent,
            Request::Remove {
                workspace: parent.workspace.id().clone(),
                operation: cleanup
            }
        )
        .await,
        Err(ToolError::Lifecycle {
            code: "hive::workspace::retry_conflict"
        })
    ));
}

#[tokio::test]
async fn native_jj_siblings_inherit_revision_intent_without_using_git_branch_semantics() {
    let fixture = Fixture::jj();
    let parent = fixture.create(fixture.jj_request()).await.unwrap();
    attach(&fixture.service, &parent).await;
    let result = call(&fixture.service, &parent, create(OperationId::new()))
        .await
        .unwrap();
    assert_eq!(result["provisioner"], "jj.workspace");
    let bad = Request::Create {
        operation: OperationId::new(),
        base_ref: None,
        branch: Some("git-branch".to_owned().try_into().unwrap()),
        revision: None,
    };
    assert!(matches!(
        call(&fixture.service, &parent, bad).await,
        Err(ToolError::Lifecycle {
            code: "hive::workspace::request"
        })
    ));
    let workspace = serde_json::from_value(result["workspace"].clone()).unwrap();
    call(
        &fixture.service,
        &parent,
        Request::Remove {
            workspace,
            operation: OperationId::new(),
        },
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn revocation_after_clone_preservation_retains_archive_evidence_without_deleting_files() {
    let fixture = Fixture::new();
    let parent = fixture.create(fixture.request()).await.unwrap();
    attach(&fixture.service, &parent).await;
    let mut request = fixture.request();
    request.policy = WorkspaceTemplate::Clone { base_ref: None };
    let child = fixture.create(request).await.unwrap();
    let operation = OperationId::new();
    let remove = Request::Remove {
        workspace: child.workspace.id().clone(),
        operation: operation.clone(),
    };
    let calls = std::sync::atomic::AtomicUsize::new(0);
    let result = fixture
        .service
        .tool_execute(
            "agent".into(),
            context(&parent),
            parent.workspace.location().unwrap().root.as_str().into(),
            tools::Request::Workspace(remove.clone()),
            move || {
                if calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= 3 {
                    Err(ToolError::Session)
                } else {
                    Ok(())
                }
            },
        )
        .await;
    assert!(matches!(
        result,
        Err(ToolError::Lifecycle {
            code: "hive::workspace::uncertain_operation"
        })
    ));
    let root = Path::new(child.workspace.location().unwrap().root.as_str());
    assert!(root.join("content").is_file());
    let persisted = fixture
        .service
        .list()
        .await
        .unwrap()
        .into_iter()
        .find(|entry| entry.workspace.id() == child.workspace.id())
        .unwrap();
    assert_eq!(persisted.workspace.state(), WorkspaceState::CleanupFailed);
    assert!(Path::new(persisted.archive.unwrap().as_str()).is_dir());
    let status = call(&fixture.service, &parent, Request::Operation { operation })
        .await
        .unwrap();
    assert_eq!(status["state"], "cleanup_failed");
    call(&fixture.service, &parent, remove).await.unwrap();
    assert!(!root.exists());
}

#[tokio::test]
async fn ephemeral_siblings_preserve_expiry_and_establish_their_own_idle_baselines() {
    for retention in [
        Retention::Until { expires_at: 1 },
        Retention::Idle {
            last_used_at: 1,
            idle_seconds: 60,
        },
    ] {
        let fixture = Fixture::new();
        let mut request = fixture.request();
        request.policy = WorkspaceTemplate::Ephemeral {
            backing: "worktree".into(),
            base_ref: None,
            retention: retention.clone(),
        };
        let parent = fixture.create(request).await.unwrap();
        attach(&fixture.service, &parent).await;
        let parent_id = parent.workspace.id().to_string();
        // A future parent baseline distinguishes copying its activity from initializing a child.
        let parent_baseline = u64::MAX - 100;
        fixture
            .service
            .run(move |rt| {
                rt.state.last_used.insert(parent_id, parent_baseline);
                rt.save()
            })
            .await
            .unwrap();
        let created = call(&fixture.service, &parent, create(OperationId::new()))
            .await
            .unwrap();
        let child_id: WorkspaceId = serde_json::from_value(created["workspace"].clone()).unwrap();
        let state = fixture
            .service
            .store
            .resource::<State>("node")
            .unwrap()
            .unwrap();
        let child = &state.workspaces[child_id.as_str()];
        assert!(matches!(
            child.workspace.intent().policy,
            WorkspacePolicy::Ephemeral { .. }
        ));
        assert_eq!(child.workspace.intent().retention, retention);
        assert_eq!(child.selection, parent.selection);
        let history = json!(state.lifecycle[child_id.as_str()]);
        let ready = history
            .as_array()
            .unwrap()
            .iter()
            .find(|transition| transition["state"] == "ready")
            .unwrap();
        assert_eq!(
            state.last_used[child_id.as_str()],
            ready["at"].as_u64().unwrap()
        );
        assert_ne!(state.last_used[child_id.as_str()], parent_baseline);
        assert_eq!(
            state.last_used[parent.workspace.id().as_str()],
            parent_baseline
        );
    }
}

#[tokio::test]
async fn git_siblings_resolve_an_explicit_base_and_branch_instead_of_the_advanced_default() {
    let fixture = Fixture::new();
    let parent = fixture.create(fixture.request()).await.unwrap();
    attach(&fixture.service, &parent).await;
    let original = git(&fixture.seed, &["rev-parse", "HEAD"], "test")
        .unwrap()
        .trim()
        .to_owned();
    std::fs::write(fixture.seed.join("content"), "advanced default\n").unwrap();
    git(&fixture.seed, &["commit", "-am", "advance default"], "test").unwrap();
    git(
        &fixture.seed,
        &["push", fixture.remote.to_str().unwrap(), "main"],
        "test",
    )
    .unwrap();
    let result = call(
        &fixture.service,
        &parent,
        Request::Create {
            operation: OperationId::new(),
            base_ref: Some(original.clone().try_into().unwrap()),
            branch: Some("explicit-sibling".to_owned().try_into().unwrap()),
            revision: None,
        },
    )
    .await
    .unwrap();
    let child = fixture
        .service
        .list()
        .await
        .unwrap()
        .into_iter()
        .find(|entry| json!(entry.workspace.id()) == result["workspace"])
        .unwrap();
    let location = child.workspace.location().unwrap();
    assert_eq!(location.resolved_base.as_ref().unwrap().as_str(), original);
    assert_eq!(
        location.branch.as_ref().unwrap().as_str(),
        "explicit-sibling"
    );
    assert_eq!(
        std::fs::read_to_string(Path::new(location.root.as_str()).join("content")).unwrap(),
        "original\n"
    );
    assert_eq!(child.selection, parent.selection);
}

#[tokio::test]
async fn jj_siblings_resolve_an_explicit_revision_instead_of_the_advanced_default() {
    let fixture = Fixture::jj();
    let parent = fixture.create(fixture.jj_request()).await.unwrap();
    attach(&fixture.service, &parent).await;
    let original = parent
        .workspace
        .location()
        .unwrap()
        .observations
        .as_ref()
        .unwrap()
        .data["base_commit"]
        .as_str()
        .unwrap()
        .to_owned();
    std::fs::write(fixture.seed.join("content"), "advanced default\n").unwrap();
    git(&fixture.seed, &["commit", "-am", "advance default"], "test").unwrap();
    git(
        &fixture.seed,
        &["push", fixture.remote.to_str().unwrap(), "main"],
        "test",
    )
    .unwrap();
    let revision = hive_core::project::Revision::try_from(original.clone()).unwrap();
    let result = call(
        &fixture.service,
        &parent,
        Request::Create {
            operation: OperationId::new(),
            base_ref: None,
            branch: None,
            revision: Some(revision.clone()),
        },
    )
    .await
    .unwrap();
    let child = fixture
        .service
        .list()
        .await
        .unwrap()
        .into_iter()
        .find(|entry| json!(entry.workspace.id()) == result["workspace"])
        .unwrap();
    let location = child.workspace.location().unwrap();
    assert_eq!(
        location.observations.as_ref().unwrap().data["base_commit"],
        original
    );
    assert_eq!(
        backends::intent(&child)
            .unwrap()
            .decode::<hive_core::project::Revision>("hive.jj.intent")
            .unwrap(),
        revision
    );
    assert_eq!(
        std::fs::read_to_string(Path::new(location.root.as_str()).join("content")).unwrap(),
        "original\n"
    );
    assert_eq!(child.selection, parent.selection);
}
