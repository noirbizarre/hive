use super::*;

#[test]
fn jj_storage_keys_preserve_uuid_suffixes_and_leave_room_for_native_windows_indexes() {
    let first =
        RepositoryId::try_from("repository-00000000-0000-0000-0000-000000000001".to_owned())
            .unwrap();
    let second =
        RepositoryId::try_from("repository-00000000-0000-0000-0000-000000000002".to_owned())
            .unwrap();
    let first = backends::jj::storage_key(&first).unwrap();
    let second = backends::jj::storage_key(&second).unwrap();
    assert_ne!(
        first, second,
        "compaction must retain UUID suffix bits, not truncate IDs"
    );
    assert!(
        first
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'v').contains(&b))
    );
    // Match the runner's ordinary state root and JJ's actual index filename length.
    let path = format!(
        r"C:\Users\runneradmin\AppData\Local\Temp\.tmp123456\state\repositories\{first}\jj.t\r\.jj\repo\index\segments\{}",
        "f".repeat(128)
    );
    assert!(
        path.len() < 260,
        "native JJ's index path exceeds MAX_PATH: {path}"
    );
}

fn command(root: &Path, args: &[&str]) -> String {
    let mut argv = vec![
        "--no-pager",
        "--color=never",
        "--config=user.name=Hive Tests",
        "--config=user.email=hive@example.test",
        "--config=signing.behavior=drop",
    ];
    argv.extend_from_slice(args);
    execute("jj", Some(root), &argv, false).unwrap()
}

fn root(entry: &Entry) -> PathBuf {
    PathBuf::from(entry.workspace.location().unwrap().root.as_str())
}

async fn cleanliness(service: &Service, entry: &Entry) -> Cleanliness {
    match service.inspect(entry.workspace.id().clone()).await.unwrap() {
        Inspection::Present { safety, .. } => safety.cleanliness,
        other => panic!("expected a managed workspace: {other:?}"),
    }
}

#[tokio::test]
async fn jj_projects_create_native_workspaces_without_worktrunk_or_git_worktrees() {
    let fixture = Fixture::jj();
    let request = fixture.jj_request();
    // Even an explicitly required WorkTrunk integration must be irrelevant to JJ.
    let result = fixture
        .service
        .create(
            request.clone(),
            fixture.definition.clone(),
            WorktrunkConfig {
                enabled: Some(true),
            },
            None,
        )
        .await;
    let entry = result.unwrap();
    let location = entry.workspace.location().unwrap();
    let root = Path::new(location.root.as_str());
    assert!(root.join(".jj").is_dir());
    assert!(!root.join(".git").exists());
    assert_eq!(
        std::fs::read_to_string(root.join("content")).unwrap(),
        "original\n"
    );
    assert!(location.branch.is_none());
    assert_eq!(
        entry.selection.as_ref().unwrap().provisioner.0,
        "jj.workspace"
    );
    assert!(matches!(
        fixture
            .service
            .inspect(entry.workspace.id().clone())
            .await
            .unwrap(),
        Inspection::Present {
            safety: SafetyEvidence {
                cleanliness: Cleanliness::Clean,
                ..
            },
            ..
        }
    ));
    let retry = fixture.create(request).await.unwrap();
    assert_eq!(retry.workspace.id(), entry.workspace.id());
}

#[tokio::test]
async fn jj_workspaces_share_storage_but_keep_independent_changes_and_survive_restart() {
    let fixture = Fixture::jj();
    let (a, b) = tokio::join!(
        fixture.create(fixture.jj_request()),
        fixture.create(fixture.jj_request())
    );
    let (a, b) = (a.unwrap(), b.unwrap());
    assert_eq!(a.repository, b.repository);
    assert_ne!(root(&a), root(&b));
    let metadata = |e: &Entry| {
        e.workspace
            .location()
            .unwrap()
            .observations
            .as_ref()
            .unwrap()
            .data
            .clone()
    };
    assert_ne!(metadata(&a)["change"], metadata(&b)["change"]);
    let reopened = Service::new(fixture.service.store.clone());
    reopened.run(|rt| rt.recover_resources()).await.unwrap();
    let attached = reopened
        .attach(a.workspace.id().clone(), "jj-session".into())
        .await
        .unwrap();
    assert_eq!(metadata(&a), metadata(&attached));
    assert!(
        reopened
            .remove(a.workspace.id().clone(), OperationId::new(), true)
            .await
            .is_err()
    );
    reopened.release("jj-session".into()).await.unwrap();
    assert_eq!(cleanliness(&reopened, &b).await, Cleanliness::Clean);
    let removed = reopened
        .remove(a.workspace.id().clone(), OperationId::new(), false)
        .await
        .unwrap();
    assert_eq!(removed.workspace.state(), WorkspaceState::Removed);
    assert!(!root(&a).exists());
    assert!(root(&b).exists());
    let repository = reopened.repositories().await.unwrap().remove(0);
    let retained = command(
        Path::new(repository.repository.root.as_str()),
        &[
            "--ignore-working-copy",
            "log",
            "--no-graph",
            "-r",
            &format!("hive-retained-{}", a.workspace.id()),
            "-T",
            "commit_id",
        ],
    );
    assert_eq!(retained.trim(), metadata(&a)["commit"].as_str().unwrap());
}

#[tokio::test]
async fn jj_cleanup_protects_unsnapshotted_snapshotted_untracked_and_ignored_data() {
    let fixture = Fixture::jj();
    for kind in ["modified", "snapshotted", "untracked", "ignored", "deleted"] {
        let entry = fixture.create(fixture.jj_request()).await.unwrap();
        let root = root(&entry);
        match kind {
            "modified" | "snapshotted" => {
                std::fs::write(root.join("content"), "valuable work\n").unwrap()
            }
            "untracked" => std::fs::write(root.join("untracked"), "valuable data").unwrap(),
            "ignored" => {
                std::fs::write(root.join(".gitignore"), "valuable\n").unwrap();
                command(&root, &["status"]);
                command(&root, &["new"]);
                std::fs::write(root.join("valuable"), "ignored data").unwrap();
            }
            "deleted" => std::fs::remove_file(root.join("content")).unwrap(),
            _ => unreachable!(),
        }
        if kind == "snapshotted" {
            command(&root, &["status"]);
        }
        let before = command(
            &root,
            &[
                "--ignore-working-copy",
                "op",
                "log",
                "--limit",
                "1",
                "--no-graph",
                "-T",
                "id",
            ],
        );
        assert_eq!(
            cleanliness(&fixture.service, &entry).await,
            Cleanliness::Dirty,
            "{kind}"
        );
        let after = command(
            &root,
            &[
                "--ignore-working-copy",
                "op",
                "log",
                "--limit",
                "1",
                "--no-graph",
                "-T",
                "id",
            ],
        );
        assert_eq!(before, after, "inspection must not snapshot {kind}");
        assert!(
            fixture
                .service
                .remove(entry.workspace.id().clone(), OperationId::new(), false)
                .await
                .is_err(),
            "{kind}"
        );
        assert!(root.exists());
        fixture
            .service
            .remove(entry.workspace.id().clone(), OperationId::new(), true)
            .await
            .unwrap();
        assert!(!root.exists());
    }
}

#[tokio::test]
async fn jj_revisions_accept_change_ids_and_revsets_but_not_git_branch_intent() {
    let fixture = Fixture::jj();
    let first = fixture.create(fixture.jj_request()).await.unwrap();
    let observations = &first
        .workspace
        .location()
        .unwrap()
        .observations
        .as_ref()
        .unwrap()
        .data;
    let change = observations["base_change"].as_str().unwrap();
    let mut request = fixture.jj_request();
    request.revision =
        Some(hive_core::project::Revision::try_from(format!("latest({change})")).unwrap());
    let second = fixture.create(request.clone()).await.unwrap();
    assert_eq!(
        second
            .workspace
            .location()
            .unwrap()
            .observations
            .as_ref()
            .unwrap()
            .data["base_commit"],
        observations["base_commit"]
    );
    request.revision = Some(hive_core::project::Revision::try_from("all()".to_owned()).unwrap());
    assert!(matches!(
        fixture.create(request).await,
        Err(Error::RetryConflict)
    ));
    for expression in ["all()", "none()", "not_a_revision"] {
        let mut request = fixture.jj_request();
        request.revision =
            Some(hive_core::project::Revision::try_from(expression.to_owned()).unwrap());
        assert!(
            matches!(fixture.create(request).await, Err(Error::JjRevision)),
            "{expression}"
        );
    }
    let mut request = fixture.jj_request();
    request.branch = Some(GitRef::try_from("branch".to_owned()).unwrap());
    assert!(matches!(fixture.create(request).await, Err(Error::Request)));
    let mut request = fixture.request();
    request.revision = Some(hive_core::project::Revision::try_from("@".to_owned()).unwrap());
    assert!(matches!(fixture.create(request).await, Err(Error::Request)));
}

#[tokio::test]
async fn jj_lost_creation_responses_settle_read_only_and_missing_markers_require_reconciliation() {
    let fixture = Fixture::jj();
    let request = fixture.jj_request();
    let entry = fixture.create(request.clone()).await.unwrap();
    let id = entry.workspace.id().clone();
    fixture
        .service
        .run(move |rt| {
            let entry = rt.state.workspaces.get_mut(id.as_str()).unwrap();
            // Reproduce a lost Ready publication after native creation and ownership marking.
            let value = serde_json::to_value(&entry.workspace).unwrap();
            let mut value = value;
            value["state"] = serde_json::json!("provisioning");
            entry.workspace = serde_json::from_value(value).unwrap();
            rt.save()
        })
        .await
        .unwrap();
    let restarted = Service::new(fixture.service.store.clone());
    let recovered = restarted
        .create(
            request,
            fixture.definition.clone(),
            WorktrunkConfig::default(),
            None,
        )
        .await
        .unwrap();
    assert_eq!(recovered.workspace.id(), entry.workspace.id());
    let id = entry.workspace.id().clone();
    restarted
        .run(move |rt| {
            let entry = rt.state.workspaces.get_mut(id.as_str()).unwrap();
            let mut value = serde_json::to_value(&entry.workspace).unwrap();
            value["state"] = serde_json::json!("provisioning");
            entry.workspace = serde_json::from_value(value).unwrap();
            rt.save()
        })
        .await
        .unwrap();
    std::fs::remove_file(root(&entry).join(".jj/hive-workspace.json")).unwrap();
    assert!(
        restarted
            .attach(entry.workspace.id().clone(), "unsafe".into())
            .await
            .is_err()
    );
    assert!(
        restarted
            .remove(entry.workspace.id().clone(), OperationId::new(), true)
            .await
            .is_err()
    );
    let reconciled = restarted
        .reconcile(entry.workspace.id().clone(), None, false)
        .await
        .unwrap();
    assert_eq!(reconciled.workspace.state(), WorkspaceState::Ready);
}

#[cfg(unix)]
#[tokio::test]
async fn jj_symlink_substitution_never_authorizes_discarding_a_foreign_directory() {
    let fixture = Fixture::jj();
    let entry = fixture.create(fixture.jj_request()).await.unwrap();
    let path = root(&entry);
    let saved = path.with_extension("saved");
    std::fs::rename(&path, &saved).unwrap();
    std::os::unix::fs::symlink(&saved, &path).unwrap();
    assert!(
        fixture
            .service
            .remove(entry.workspace.id().clone(), OperationId::new(), true)
            .await
            .is_err()
    );
    assert!(saved.join("content").exists());
}

#[cfg(unix)]
fn fake_jj(fixture: &Fixture, contents: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = fixture._directory.path().join("fake-jj");
    std::fs::write(&path, contents).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[cfg(unix)]
#[tokio::test]
async fn jj_successful_commands_with_unreadable_content_warnings_never_prove_an_empty_file_clean() {
    let fixture = Fixture::jj();
    let entry = fixture.create(fixture.jj_request()).await.unwrap();
    std::fs::write(root(&entry).join("content"), "").unwrap();
    fake_jj(
        &fixture,
        "#!/bin/sh\ncase \"$*\" in *'file show'*) printf 'unreadable content secret\\n' >&2; exit 0;; esac\nexec jj \"$@\"\n",
    );
    assert_eq!(
        cleanliness(&fixture.service, &entry).await,
        Cleanliness::Unknown
    );
    assert!(
        fixture
            .service
            .remove(entry.workspace.id().clone(), OperationId::new(), true)
            .await
            .is_err()
    );
    assert!(root(&entry).join("content").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn jj_missing_old_and_incompatible_commands_fail_before_repository_effects_without_leaking_output()
 {
    for script in [
        "#!/bin/sh\nexit 127\n",
        "#!/bin/sh\nprintf 'jj 0.45.0 secret\\n'\n",
        "#!/bin/sh\nprintf 'jj 0.46.0 secret\\n'\n",
    ] {
        let fixture = Fixture::jj();
        fake_jj(&fixture, script);
        let result = fixture.create(fixture.jj_request()).await.unwrap_err();
        assert!(matches!(result, Error::JjUnavailable));
        assert!(!result.to_string().contains("secret"));
        let repository = fixture.service.repositories().await.unwrap().remove(0);
        assert!(!Path::new(repository.repository.root.as_str()).exists());
    }
}

#[cfg(unix)]
#[tokio::test]
async fn jj_explicit_cleanup_retry_finishes_lost_native_deregistration_but_startup_never_does() {
    let fixture = Fixture::jj();
    let entry = fixture.create(fixture.jj_request()).await.unwrap();
    let script = fake_jj(
        &fixture,
        "#!/bin/sh\ncase \"$*\" in *'workspace forget'*) exit 1;; esac\nexec jj \"$@\"\n",
    );
    let operation = OperationId::new();
    assert!(
        fixture
            .service
            .remove(entry.workspace.id().clone(), operation.clone(), false)
            .await
            .is_err()
    );
    assert!(!root(&entry).exists());
    std::fs::remove_file(script).unwrap();
    let restarted = Service::new(fixture.service.store.clone());
    restarted.run(|rt| rt.recover_resources()).await.unwrap();
    let repository = restarted.repositories().await.unwrap().remove(0);
    let registrations = command(
        Path::new(repository.repository.root.as_str()),
        &[
            "--ignore-working-copy",
            "workspace",
            "list",
            "-T",
            "name ++ '\\n'",
        ],
    );
    assert!(registrations.contains(entry.workspace.id().as_str()));
    let removed = restarted
        .remove(entry.workspace.id().clone(), operation, false)
        .await
        .unwrap();
    assert_eq!(removed.workspace.state(), WorkspaceState::Removed);
    let registrations = command(
        Path::new(repository.repository.root.as_str()),
        &[
            "--ignore-working-copy",
            "workspace",
            "list",
            "-T",
            "name ++ '\\n'",
        ],
    );
    assert!(!registrations.contains(entry.workspace.id().as_str()));
}

#[tokio::test]
async fn jj_fetch_advances_new_bases_without_moving_existing_workspaces_and_trunk_is_the_default() {
    let mut fixture = Fixture::jj();
    fixture.definition.default_revision = None;
    let first = fixture.create(fixture.jj_request()).await.unwrap();
    std::fs::write(fixture.seed.join("content"), "upstream\n").unwrap();
    git(&fixture.seed, &["add", "content"], "test").unwrap();
    git(&fixture.seed, &["commit", "-m", "upstream"], "test").unwrap();
    git(
        &fixture.seed,
        &["push", fixture.remote.to_str().unwrap(), "main"],
        "test",
    )
    .unwrap();
    let second = fixture.create(fixture.jj_request()).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(root(&first).join("content")).unwrap(),
        "original\n"
    );
    assert_eq!(
        std::fs::read_to_string(root(&second).join("content")).unwrap(),
        "upstream\n"
    );
}

#[tokio::test]
async fn jj_ephemeral_workspaces_use_the_native_policy_and_generic_retention() {
    let fixture = Fixture::jj();
    let mut request = fixture.jj_request();
    request.policy = WorkspaceTemplate::Ephemeral {
        backing: "jj".into(),
        base_ref: None,
        retention: Retention::Until {
            expires_at: u64::MAX,
        },
    };
    let entry = fixture.create(request).await.unwrap();
    assert_eq!(entry.selection.unwrap().provisioner.0, "jj.workspace");
}

#[cfg(unix)]
#[tokio::test]
async fn jj_interrupted_acquisition_reuses_its_owned_staging_and_repository_identity() {
    let fixture = Fixture::jj();
    let script = fake_jj(
        &fixture,
        "#!/bin/sh\ncase \"$*\" in *'git clone'*) jj \"$@\"; exit 1;; esac\nexec jj \"$@\"\n",
    );
    let request = fixture.jj_request();
    assert!(fixture.create(request.clone()).await.is_err());
    let original = fixture.service.repositories().await.unwrap().remove(0);
    let workspace = fixture.service.list().await.unwrap().remove(0);
    std::fs::remove_file(script).unwrap();
    let restarted = Service::new(fixture.service.store.clone());
    let entry = restarted
        .create(
            request,
            fixture.definition.clone(),
            WorktrunkConfig::default(),
            None,
        )
        .await
        .unwrap();
    assert_eq!(entry.workspace.id(), workspace.workspace.id());
    assert_eq!(entry.repository.as_ref(), Some(&original.repository.id));
    assert_eq!(
        std::fs::read_to_string(root(&entry).join("content")).unwrap(),
        "original\n"
    );
}

#[tokio::test]
async fn jj_failed_cleanup_checkpoints_prevent_filesystem_removal() {
    struct Refuse;
    impl hive_core::workspace::backend::CheckpointWriter for Refuse {
        type Error = Error;
        fn checkpoint(&mut self, _data: BackendData) -> Result<(), Error> {
            Err(Error::Storage)
        }
    }
    let fixture = Fixture::jj();
    let request = fixture.jj_request();
    let entry = fixture.create(request.clone()).await.unwrap();
    let operation = fixture.service.operation(request.operation).await.unwrap();
    let prepared: BackendData = serde_json::from_value(operation["prepared"].clone()).unwrap();
    let selection = entry.selection.clone().unwrap();
    let result = fixture
        .service
        .manager
        .provisioner(&selection)
        .unwrap()
        .remove_checkpointed(
            selection.repository.as_ref(),
            &prepared,
            false,
            &OperationId::new(),
            &mut Refuse,
        );
    assert!(matches!(result, Err(Error::Storage)));
    assert_eq!(
        cleanliness(&fixture.service, &entry).await,
        Cleanliness::Clean
    );
    assert!(root(&entry).join("content").exists());
}

#[tokio::test]
async fn jj_cleanliness_streams_large_binary_files_and_ignores_user_output_templates() {
    let fixture = Fixture::jj();
    let entry = fixture.create(fixture.jj_request()).await.unwrap();
    let root = root(&entry);
    let binary = root.join("binary");
    std::fs::write(&binary, vec![255; 2 * 1024 * 1024]).unwrap();
    // JJ normally limits auto-tracking large files; this is deliberate user work for the fixture.
    command(&root, &["--config=snapshot.max-new-file-size=0", "status"]);
    command(&root, &["new"]);
    command(
        &root,
        &[
            "config",
            "set",
            "--repo",
            "templates.file_show",
            "'misleading metadata'",
        ],
    );
    assert_eq!(
        cleanliness(&fixture.service, &entry).await,
        Cleanliness::Clean
    );
    std::fs::write(&binary, vec![254; 2 * 1024 * 1024]).unwrap();
    assert_eq!(
        cleanliness(&fixture.service, &entry).await,
        Cleanliness::Dirty
    );
}

#[cfg(unix)]
#[tokio::test]
async fn jj_cleanliness_checks_tracked_symlinks_and_executable_modes_without_following_links() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::jj();
    let entry = fixture.create(fixture.jj_request()).await.unwrap();
    let root = root(&entry);
    std::os::unix::fs::symlink("content", root.join("link")).unwrap();
    command(&root, &["status"]);
    command(&root, &["new"]);
    assert_eq!(
        cleanliness(&fixture.service, &entry).await,
        Cleanliness::Clean
    );
    std::fs::set_permissions(root.join("content"), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        cleanliness(&fixture.service, &entry).await,
        Cleanliness::Dirty
    );
    std::fs::set_permissions(root.join("content"), std::fs::Permissions::from_mode(0o644)).unwrap();
    std::fs::remove_file(root.join("link")).unwrap();
    std::os::unix::fs::symlink("/outside", root.join("link")).unwrap();
    assert_eq!(
        cleanliness(&fixture.service, &entry).await,
        Cleanliness::Dirty
    );
}

#[tokio::test]
async fn jj_foreign_node_requests_are_rejected_before_native_commands_or_filesystem_effects() {
    use hive_core::workspace::{
        WorkspaceError,
        backend::{ProvisionRequest, WorkspaceBackend},
    };
    let fixture = Fixture::jj();
    let entry = fixture.create(fixture.jj_request()).await.unwrap();
    let backend: &dyn WorkspaceBackend = &fixture.service;
    let target = ResourceTarget {
        workspace: entry.workspace.id().clone(),
        node: NodeId::new(),
        backend: entry.workspace.backend().unwrap().clone(),
        ownership: Ownership::HiveManaged,
        location: entry.workspace.location().cloned(),
    };
    assert!(matches!(
        backend.inspect(target.clone()).await,
        Err(WorkspaceError::NodeMismatch { .. })
    ));
    assert!(
        backend
            .cleanup(CleanupRequest {
                operation: OperationId::new(),
                target,
                mode: CleanupMode::DiscardChanges
            })
            .await
            .is_err()
    );
    let mut intent = entry.workspace.intent().clone();
    intent.node = NodeId::new();
    assert!(
        backend
            .provision(ProvisionRequest {
                workspace: entry.workspace.id().clone(),
                operation: entry.workspace.operation().unwrap().clone(),
                backend: entry.workspace.backend().unwrap().clone(),
                intent,
            })
            .await
            .is_err()
    );
    assert_eq!(
        std::fs::read_to_string(root(&entry).join("content")).unwrap(),
        "original\n"
    );
}
