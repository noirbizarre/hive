use super::*;

#[tokio::test]
async fn workspace_crash_subprocess_host() {
    let Ok(database) = std::env::var("HIVE_WORKSPACE_CRASH_DATABASE") else {
        return;
    };
    let service = Service::new(Arc::new(Store::open(database.into()).unwrap()));
    match std::env::var("HIVE_WORKSPACE_CRASH_PHASE")
        .unwrap()
        .as_str()
    {
        "provisioned" => {
            let request =
                serde_json::from_str(&std::env::var("HIVE_WORKSPACE_CRASH_REQUEST").unwrap())
                    .unwrap();
            let definition =
                serde_json::from_str(&std::env::var("HIVE_WORKSPACE_CRASH_DEFINITION").unwrap())
                    .unwrap();
            service
                .create(
                    request,
                    definition,
                    WorktrunkConfig {
                        enabled: Some(false),
                    },
                    None,
                )
                .await
                .unwrap();
        }
        "planned" => {
            service
                .plan_sweep(SweepRequest {
                    operation: OperationId::new(),
                    workspaces: None,
                })
                .await
                .unwrap();
        }
        "removed" | "deleted" => {
            service
                .apply_sweep(
                    OperationId::try_from(std::env::var("HIVE_WORKSPACE_CRASH_SWEEP").unwrap())
                        .unwrap(),
                )
                .await
                .unwrap();
        }
        _ => panic!("unknown crash phase"),
    }
}

#[tokio::test]
async fn process_death_preserves_creation_and_sweep_boundaries_without_resuming_deletion() {
    use tokio::io::{AsyncBufReadExt, BufReader};
    for (phase, backing) in [
        ("provisioned", "clone"),
        ("provisioned", "worktree"),
        ("planned", "clone"),
        ("removed", "clone"),
        ("removed", "worktree"),
        ("deleted", "clone"),
        ("deleted", "worktree"),
    ] {
        let fixture = Fixture::new();
        let entry = if phase == "provisioned" {
            None
        } else {
            Some(expired(&fixture, backing).await)
        };
        let sweep = if matches!(phase, "removed" | "deleted") {
            Some(plan(&fixture.service, None).await)
        } else {
            None
        };
        let mut request = fixture.request();
        request.policy = WorkspaceTemplate::Ephemeral {
            backing: backing.into(),
            base_ref: None,
            retention: Retention::Until { expires_at: 1 },
        };
        let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "workspaces::tests::recovery_tests::workspace_crash_subprocess_host",
                "--nocapture",
            ])
            .env(
                "HIVE_WORKSPACE_CRASH_DATABASE",
                fixture
                    .service
                    .store
                    .state_directory()
                    .join("catalog.sqlite3"),
            )
            .env("HIVE_WORKSPACE_CRASH_PHASE", phase)
            .env(
                "HIVE_WORKSPACE_CRASH_REQUEST",
                serde_json::to_string(&request).unwrap(),
            )
            .env(
                "HIVE_WORKSPACE_CRASH_DEFINITION",
                serde_json::to_string(&fixture.definition).unwrap(),
            )
            .env(
                "LLVM_PROFILE_FILE",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            )
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        if let Some(sweep) = &sweep {
            command.env(
                "HIVE_WORKSPACE_CRASH_SWEEP",
                sweep["request"]["operation"].as_str().unwrap(),
            );
        }
        let mut child = command.spawn().unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let line = lines
                    .next_line()
                    .await
                    .unwrap()
                    .expect("child exited before crash boundary");
                if line == "HIVE_WORKSPACE_CRASH_READY" {
                    break;
                }
            }
        })
        .await
        .unwrap();
        child.kill().await.unwrap();
        child.wait().await.unwrap();
        let restarted = Service::new(fixture.service.store.clone());
        if phase == "deleted" {
            // This distinct boundary proves startup settles Cleaning, not merely a stale batch result.
            let interrupted = restarted.list().await.unwrap();
            assert_eq!(interrupted[0].workspace.state(), WorkspaceState::Cleaning);
            assert!(
                !Path::new(interrupted[0].workspace.location().unwrap().root.as_str()).exists()
            );
        }
        restarted.run(|rt| rt.recover_resources()).await.unwrap();
        restarted.run(|rt| rt.recover_resources()).await.unwrap();
        let records = restarted.list().await.unwrap();
        assert_eq!(records.len(), 1);
        let record = &records[0];
        if matches!(phase, "removed" | "deleted") {
            assert_eq!(record.workspace.state(), WorkspaceState::Removed);
            let result = apply(&restarted, sweep.as_ref().unwrap()).await;
            assert_eq!(result["items"][0]["status"], "already_removed");
            assert_eq!(
                result["items"][0]["cleanup"],
                sweep.as_ref().unwrap()["items"][0]["cleanup"]
            );
        } else {
            assert_eq!(record.workspace.state(), WorkspaceState::Ready);
            assert!(Path::new(record.workspace.location().unwrap().root.as_str()).exists());
            if phase == "planned" {
                let stored: serde_json::Value = restarted.store.resource("node").unwrap().unwrap();
                let sweep = stored["sweeps"]
                    .as_object()
                    .unwrap()
                    .values()
                    .next()
                    .unwrap();
                assert_eq!(sweep["items"][0]["status"], "planned");
                assert_eq!(
                    record.workspace.id(),
                    entry.as_ref().unwrap().workspace.id()
                );
                assert_eq!(
                    apply(&restarted, sweep).await["items"][0]["status"],
                    "removed"
                );
            } else {
                assert_eq!(record.workspace.operation(), Some(&request.operation));
            }
        }
    }
}

async fn expired(fixture: &Fixture, backing: &str) -> Entry {
    let mut request = fixture.request();
    request.policy = WorkspaceTemplate::Ephemeral {
        backing: backing.into(),
        base_ref: None,
        retention: Retention::Until { expires_at: 1 },
    };
    fixture.create(request).await.unwrap()
}

async fn plan(service: &Service, ids: Option<Vec<WorkspaceId>>) -> serde_json::Value {
    serde_json::to_value(
        service
            .plan_sweep(SweepRequest {
                operation: OperationId::new(),
                workspaces: ids,
            })
            .await
            .unwrap(),
    )
    .unwrap()
}

async fn apply(service: &Service, sweep: &serde_json::Value) -> serde_json::Value {
    serde_json::to_value(
        service
            .apply_sweep(
                OperationId::try_from(sweep["request"]["operation"].as_str().unwrap().to_owned())
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .unwrap()
}

#[tokio::test]
async fn sweep_freezes_candidates_and_preserves_dirty_active_unknown_and_persistent_resources() {
    let fixture = Fixture::new();
    let persistent = fixture.create(fixture.request()).await.unwrap();
    let first = expired(&fixture, "worktree").await;
    let clone = expired(&fixture, "clone").await;
    let sweep = plan(&fixture.service, None).await;
    assert_eq!(sweep["items"].as_array().unwrap().len(), 2);
    let late = expired(&fixture, "worktree").await;
    let first_path = PathBuf::from(first.workspace.location().unwrap().root.as_str());
    std::fs::write(first_path.join("untracked"), "do not lose").unwrap();
    fixture
        .service
        .attach(clone.workspace.id().clone(), "uncertain-session".into())
        .await
        .unwrap();
    fixture
        .service
        .mark_dispatched("uncertain-session".into())
        .await
        .unwrap();
    let applied = apply(&fixture.service, &sweep).await;
    assert!(
        applied["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["status"] == "blocked")
    );
    assert_eq!(
        applied["items"][0]["operation"],
        sweep["items"][0]["operation"]
    );
    fixture
        .service
        .run(|rt| rt.recover_resources())
        .await
        .unwrap();
    assert!(first_path.join("untracked").is_file());
    assert!(Path::new(late.workspace.location().unwrap().root.as_str()).exists());
    assert!(Path::new(persistent.workspace.location().unwrap().root.as_str()).exists());
    std::fs::remove_file(first_path.join("untracked")).unwrap();
    fixture
        .service
        .release("uncertain-session".into())
        .await
        .unwrap();
    let (one, two) = tokio::join!(
        apply(&fixture.service, &sweep),
        apply(&fixture.service, &sweep)
    );
    for result in [one, two] {
        assert!(
            result["items"]
                .as_array()
                .unwrap()
                .iter()
                .all(|item| matches!(item["status"].as_str(), Some("removed" | "already_removed")))
        );
        assert_eq!(result["items"][0]["cleanup"], sweep["items"][0]["cleanup"]);
    }
    assert!(!first_path.exists());
    let clone = fixture
        .service
        .list()
        .await
        .unwrap()
        .into_iter()
        .find(|entry| entry.workspace.id() == clone.workspace.id())
        .unwrap();
    assert!(Path::new(clone.archive.as_ref().unwrap().as_str()).is_dir());
    let persistent_plan = plan(
        &fixture.service,
        Some(vec![persistent.workspace.id().clone()]),
    )
    .await;
    assert_eq!(
        apply(&fixture.service, &persistent_plan).await["items"][0]["status"],
        "blocked"
    );
}

#[tokio::test]
async fn sweep_retries_failed_cleanup_with_the_original_request_after_observations_change() {
    let fixture = Fixture::new();
    let entry = expired(&fixture, "worktree").await;
    let path = PathBuf::from(entry.workspace.location().unwrap().root.as_str());
    let sweep = plan(&fixture.service, None).await;
    let lock_path = path.clone();
    fixture
        .service
        .run(move |_| {
            git(
                &lock_path,
                &["worktree", "lock", lock_path.to_str().unwrap()],
                "test",
            )
            .map(|_| ())
        })
        .await
        .unwrap();
    let failed = apply(&fixture.service, &sweep).await;
    assert_eq!(failed["items"][0]["status"], "retryable_failure");
    let updated = path.clone();
    fixture
        .service
        .run(move |rt| {
            git(
                &updated,
                &["worktree", "unlock", updated.to_str().unwrap()],
                "test",
            )?;
            git(
                &updated,
                &["commit", "--allow-empty", "-m", "new observation"],
                "test",
            )?;
            // Simulate a later authoritative location observation without altering cleanup intent.
            let observed = rt.location(&entry, &updated, None)?;
            let record = rt
                .state
                .workspaces
                .get_mut(entry.workspace.id().as_str())
                .unwrap();
            let mut value = serde_json::to_value(&record.workspace).unwrap();
            value["location"] = serde_json::to_value(observed).unwrap();
            record.workspace = serde_json::from_value(value).unwrap();
            rt.save()?;
            rt.reconcile(entry.workspace.id().clone(), None, false)
                .map(|_| ())
        })
        .await
        .unwrap();
    let replacement = plan(&fixture.service, None).await;
    assert_eq!(replacement["items"][0]["status"], "blocked");
    assert_eq!(
        replacement["items"][0]["failure"]["code"],
        "hive::workspace::cleanup_in_progress"
    );
    assert_eq!(
        replacement["items"][0]["failure"]["operation"],
        sweep["items"][0]["operation"]
    );
    let done = apply(&fixture.service, &sweep).await;
    assert_eq!(done["items"][0]["status"], "removed");
    assert_eq!(
        done["items"][0]["operation"],
        sweep["items"][0]["operation"]
    );
    assert_eq!(done["items"][0]["cleanup"], sweep["items"][0]["cleanup"]);
}

#[tokio::test]
async fn recovery_reports_orphans_and_partial_operations_without_claiming_or_deleting_them() {
    let fixture = Fixture::new();
    let entry = expired(&fixture, "worktree").await;
    let root = PathBuf::from(entry.workspace.location().unwrap().root.as_str());
    let unrecorded = root.parent().unwrap().join("unrecorded");
    std::fs::create_dir(&unrecorded).unwrap();
    std::fs::write(unrecorded.join("important"), "keep").unwrap();
    let id = entry.workspace.id().clone();
    fixture
        .service
        .run(move |rt| {
            let record = rt.state.workspaces.get_mut(id.as_str()).unwrap();
            let mut workspace = Workspace::new(id, record.workspace.intent().clone())?;
            workspace.transition(WorkspaceEvent::Provision {
                operation: record.workspace.operation().unwrap().clone(),
                backend: record.workspace.backend().unwrap().clone(),
            })?;
            git(
                &root,
                &["config", "--worktree", "--unset", "hive.workspace"],
                "test",
            )?;
            record.workspace = workspace;
            rt.save()
        })
        .await
        .unwrap();
    for _ in 0..2 {
        fixture
            .service
            .run(|rt| rt.recover_resources())
            .await
            .unwrap();
    }
    let report =
        serde_json::to_value(fixture.service.recovery(BTreeSet::new()).await.unwrap()).unwrap();
    assert!(
        report["orphans"]
            .as_array()
            .unwrap()
            .iter()
            .any(|orphan| orphan["path"] == unrecorded.to_str().unwrap())
    );
    assert!(
        report["workspaces"][0]["classifications"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("incomplete_creation"))
    );
    assert!(unrecorded.join("important").exists());
    assert_eq!(
        fixture.service.list().await.unwrap()[0].workspace.state(),
        WorkspaceState::Provisioning
    );
    fixture
        .service
        .reconcile(entry.workspace.id().clone(), None, false)
        .await
        .unwrap();
    assert_eq!(
        fixture.service.list().await.unwrap()[0].workspace.state(),
        WorkspaceState::Ready
    );
}

#[tokio::test]
async fn restart_settles_proven_native_creation_once_without_replaying_effects() {
    let fixture = Fixture::new();
    let entry = expired(&fixture, "clone").await;
    let id = entry.workspace.id().clone();
    fixture
        .service
        .run(move |rt| {
            let entry = rt.state.workspaces.get_mut(id.as_str()).unwrap();
            let mut interrupted = Workspace::new(id, entry.workspace.intent().clone())?;
            interrupted.transition(WorkspaceEvent::Provision {
                operation: entry.workspace.operation().unwrap().clone(),
                backend: entry.workspace.backend().unwrap().clone(),
            })?;
            entry.workspace = interrupted;
            rt.save()
        })
        .await
        .unwrap();
    let restarted = Service::new(fixture.service.store.clone());
    restarted.run(|rt| rt.recover_resources()).await.unwrap();
    let before: serde_json::Value = restarted.store.resource("node").unwrap().unwrap();
    restarted.run(|rt| rt.recover_resources()).await.unwrap();
    let after: serde_json::Value = restarted.store.resource("node").unwrap().unwrap();
    assert_eq!(before, after);
    assert_eq!(
        restarted.list().await.unwrap()[0].workspace.location(),
        entry.workspace.location()
    );
}

#[tokio::test]
async fn idle_retention_baselines_survive_legacy_loading_alias_release_and_clock_rollback() {
    let fixture = Fixture::new();
    let mut request = fixture.request();
    request.policy = WorkspaceTemplate::Ephemeral {
        backing: "worktree".into(),
        base_ref: None,
        retention: Retention::Idle {
            last_used_at: 1,
            idle_seconds: 60,
        },
    };
    let entry = fixture.create(request).await.unwrap();
    let id = entry.workspace.id().to_string();
    let mut legacy: serde_json::Value = fixture.service.store.resource("node").unwrap().unwrap();
    legacy.as_object_mut().unwrap().remove("last_used");
    legacy.as_object_mut().unwrap().remove("lifecycle");
    legacy.as_object_mut().unwrap().remove("sweeps");
    fixture
        .service
        .store
        .save_resource("node", &legacy)
        .unwrap();
    fixture
        .service
        .run(|rt| rt.recover_resources())
        .await
        .unwrap();
    let report =
        serde_json::to_value(fixture.service.recovery(BTreeSet::new()).await.unwrap()).unwrap();
    assert_eq!(report["workspaces"][0]["retention_eligible"], false);
    let baseline = report["workspaces"][0]["last_used_at"].as_u64().unwrap();
    let key = id.clone();
    fixture
        .service
        .run(move |rt| {
            rt.state.last_used.insert(key, 1);
            rt.save()
        })
        .await
        .unwrap();
    let stale = plan(&fixture.service, None).await;
    assert_eq!(stale["items"].as_array().unwrap().len(), 1);
    fixture
        .service
        .track_directory(
            PathBuf::from(entry.workspace.location().unwrap().root.as_str()),
            "earlier-alias".into(),
        )
        .await
        .unwrap();
    let key = id.clone();
    fixture
        .service
        .run(move |rt| {
            rt.state.last_used.insert(key, 1);
            rt.save()
        })
        .await
        .unwrap();
    assert_eq!(
        apply(&fixture.service, &stale).await["items"][0]["status"],
        "blocked"
    );
    fixture
        .service
        .release("earlier-alias".into())
        .await
        .unwrap();
    assert!(
        plan(&fixture.service, None).await["items"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let future = baseline + 1000;
    let key = id.clone();
    fixture
        .service
        .run(move |rt| {
            rt.state.last_used.insert(key, future);
            rt.save()
        })
        .await
        .unwrap();
    fixture
        .service
        .track_directory(
            PathBuf::from(entry.workspace.location().unwrap().root.as_str()),
            "alias".into(),
        )
        .await
        .unwrap();
    fixture.service.release("alias".into()).await.unwrap();
    fixture
        .service
        .run(|rt| rt.recover_resources())
        .await
        .unwrap();
    let state: serde_json::Value = fixture.service.store.resource("node").unwrap().unwrap();
    assert_eq!(state["last_used"][&id], future);
    assert!(
        plan(&fixture.service, None).await["items"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let mut overflow = fixture.request();
    overflow.policy = WorkspaceTemplate::Ephemeral {
        backing: "worktree".into(),
        base_ref: None,
        retention: Retention::Idle {
            last_used_at: 1,
            idle_seconds: u64::MAX,
        },
    };
    fixture.create(overflow).await.unwrap();
    assert!(
        plan(&fixture.service, None).await["items"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_dangling_symlink_is_not_proven_cleanup_absence() {
    let fixture = Fixture::new();
    let entry = expired(&fixture, "clone").await;
    let path = PathBuf::from(entry.workspace.location().unwrap().root.as_str());
    let sweep = plan(&fixture.service, None).await;
    assert_eq!(
        apply(&fixture.service, &sweep).await["items"][0]["status"],
        "removed"
    );
    std::os::unix::fs::symlink(path.with_extension("missing"), &path).unwrap();
    assert!(matches!(
        fixture
            .service
            .inspect(entry.workspace.id().clone())
            .await
            .unwrap(),
        Inspection::IdentityConflict
    ));
    assert_eq!(
        apply(&fixture.service, &sweep).await["items"][0]["status"],
        "manual_intervention"
    );
    assert!(path.symlink_metadata().unwrap().file_type().is_symlink());
}

#[cfg(unix)]
#[tokio::test]
async fn recovery_reports_symlink_and_truncated_scans_without_traversing_unknown_content() {
    let fixture = Fixture::new();
    let entry = expired(&fixture, "worktree").await;
    let state = fixture.service.store.state_directory();
    std::os::unix::fs::symlink(&fixture.seed, state.join("clones")).unwrap();
    let report =
        serde_json::to_value(fixture.service.recovery(BTreeSet::new()).await.unwrap()).unwrap();
    assert!(
        report["scan_errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["code"] == "hive::workspace::scan_symlink")
    );
    assert!(!report["orphans"].as_array().unwrap().iter().any(|item| {
        item["path"]
            .as_str()
            .unwrap()
            .starts_with(fixture.seed.to_str().unwrap())
    }));
    let directory = Path::new(entry.workspace.location().unwrap().root.as_str())
        .parent()
        .unwrap();
    for index in 0..4097 {
        std::fs::create_dir(directory.join(format!("orphan-{index}"))).unwrap();
    }
    let report =
        serde_json::to_value(fixture.service.recovery(BTreeSet::new()).await.unwrap()).unwrap();
    assert!(
        report["scan_errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["code"] == "hive::workspace::scan_truncated")
    );
    assert!(directory.join("orphan-4096").exists());
}

#[tokio::test]
async fn sweep_identities_cannot_be_reused_with_changed_selection_or_create_intent() {
    let fixture = Fixture::new();
    let entry = expired(&fixture, "worktree").await;
    let operation = OperationId::new();
    let request = SweepRequest {
        operation: operation.clone(),
        workspaces: Some(vec![
            entry.workspace.id().clone(),
            entry.workspace.id().clone(),
        ]),
    };
    let original =
        serde_json::to_value(fixture.service.plan_sweep(request.clone()).await.unwrap()).unwrap();
    assert_eq!(
        original,
        serde_json::to_value(fixture.service.plan_sweep(request).await.unwrap()).unwrap()
    );
    assert!(matches!(
        fixture
            .service
            .plan_sweep(SweepRequest {
                operation: operation.clone(),
                workspaces: None
            })
            .await,
        Err(Error::RetryConflict)
    ));
    for operation in [
        operation,
        OperationId::try_from(
            original["items"][0]["operation"]
                .as_str()
                .unwrap()
                .to_owned(),
        )
        .unwrap(),
    ] {
        let mut create = fixture.request();
        create.operation = operation;
        assert!(matches!(
            fixture.create(create).await,
            Err(Error::RetryConflict)
        ));
    }
}

#[tokio::test]
async fn corrupt_archives_do_not_turn_missing_clone_cleanup_into_proven_completion() {
    let fixture = Fixture::new();
    let entry = expired(&fixture, "clone").await;
    let id = entry.workspace.id().clone();
    let sweep = plan(&fixture.service, None).await;
    let done = apply(&fixture.service, &sweep).await;
    assert_eq!(done["items"][0]["status"], "removed");
    fixture
        .service
        .run(move |rt| {
            let record = rt.state.workspaces.get_mut(id.as_str()).unwrap();
            let mut interrupted = entry.workspace;
            interrupted.transition(WorkspaceEvent::Cleanup {
                operation: record.workspace.operation().unwrap().clone(),
            })?;
            git(
                Path::new(record.archive.as_ref().unwrap().as_str()),
                &["config", "hive.archiveOperation", "foreign-operation"],
                "test",
            )?;
            record.workspace = interrupted;
            rt.save()
        })
        .await
        .unwrap();
    fixture
        .service
        .run(|rt| rt.recover_resources())
        .await
        .unwrap();
    assert_eq!(
        fixture.service.list().await.unwrap()[0].workspace.state(),
        WorkspaceState::Cleaning
    );
    assert_ne!(
        apply(&fixture.service, &sweep).await["items"][0]["status"],
        "removed"
    );
}
