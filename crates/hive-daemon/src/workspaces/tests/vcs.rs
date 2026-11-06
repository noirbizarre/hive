use super::*;
use hive_core::workspace::backend::vcs::{self as api, DiffView, Inclusion, Request};

async fn call(
    fixture: &Fixture,
    entry: &Entry,
    request: Request,
) -> Result<api::Result, api::Error> {
    let entry = entry.clone();
    fixture
        .service
        .run(move |rt| {
            let selection = rt.selection(&entry)?;
            let repository = selection.repository.as_ref().unwrap();
            let backend = rt.manager.repository(repository)?;
            Ok(backend.vcs(
                api::Target {
                    repository,
                    root: &entry.workspace.location().unwrap().root,
                    authorize_mutation: &|| Ok(()),
                },
                &request,
            ))
        })
        .await
        .unwrap()
}

fn commit(inclusion: Inclusion) -> Request {
    Request::Commit {
        message: "record agent work".into(),
        inclusion,
        untracked: vec![],
        ignored: vec![],
        expected_state: None,
    }
}

#[tokio::test]
async fn git_staged_commits_preserve_partial_staging_and_untracked_files() {
    let fixture = Fixture::new();
    let entry = fixture.create(fixture.request()).await.unwrap();
    let root = Path::new(entry.workspace.location().unwrap().root.as_str());
    std::fs::write(root.join("content"), "staged\n").unwrap();
    git(root, &["add", "content"], "test").unwrap();
    std::fs::write(root.join("content"), "unstaged\n").unwrap();
    std::fs::write(root.join("new file"), "untracked\n").unwrap();
    let before = call(&fixture, &entry, Request::Status {}).await.unwrap();
    assert!(
        before
            .state
            .files
            .iter()
            .any(|file| file.index.is_some() && file.working_copy.is_some())
    );
    let diff = call(
        &fixture,
        &entry,
        Request::Diff {
            view: DiffView::Staged,
            max_bytes: 65536,
        },
    )
    .await
    .unwrap();
    assert!(diff.diff[0].text.contains("+staged"));
    assert!(!diff.diff[0].text.contains("+unstaged"));
    let result = call(&fixture, &entry, commit(Inclusion::StagedOnly))
        .await
        .unwrap();
    assert_eq!(
        git(root, &["show", "HEAD:content"], "test").unwrap(),
        "staged\n"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("content")).unwrap(),
        "unstaged\n"
    );
    assert!(
        result
            .state
            .files
            .iter()
            .any(|file| file.path == "new file" && file.untracked)
    );
    assert_eq!(
        result.recorded.unwrap().identity.commit_id,
        git(root, &["rev-parse", "HEAD"], "test").unwrap().trim()
    );
}

#[tokio::test]
async fn git_state_tokens_distinguish_equal_revision_branches_and_empty_net_tracked_changes() {
    let fixture = Fixture::new();
    let entry = fixture.create(fixture.request()).await.unwrap();
    let root = Path::new(entry.workspace.location().unwrap().root.as_str());
    let initial = call(&fixture, &entry, Request::Status {})
        .await
        .unwrap()
        .state;
    git(root, &["checkout", "-b", "same-revision"], "test").unwrap();
    let switched = call(&fixture, &entry, Request::Status {})
        .await
        .unwrap()
        .state;
    assert_eq!(initial.identity.commit_id, switched.identity.commit_id);
    assert_ne!(initial.state_token, switched.state_token);
    std::fs::write(root.join("content"), "staged\n").unwrap();
    git(root, &["add", "content"], "test").unwrap();
    std::fs::write(root.join("content"), "original\n").unwrap();
    assert_eq!(
        call(&fixture, &entry, commit(Inclusion::TrackedWorkingCopy))
            .await
            .unwrap_err(),
        api::Error::Empty
    );
}

#[tokio::test]
async fn git_status_preserves_literal_rename_paths_and_read_commands_leave_the_index_unchanged() {
    let fixture = Fixture::new();
    let entry = fixture.create(fixture.request()).await.unwrap();
    let root = Path::new(entry.workspace.location().unwrap().root.as_str());
    let renamed = if cfg!(windows) {
        "renamed [file]"
    } else {
        "renamed [file]\nwith newline"
    };
    git(root, &["mv", "content", renamed], "test").unwrap();
    let index = git(root, &["rev-parse", "--git-path", "index"], "test").unwrap();
    let index = root.join(index.trim());
    let before = std::fs::read(&index).unwrap();
    let status = call(&fixture, &entry, Request::Status {}).await.unwrap();
    let rename = status
        .state
        .files
        .iter()
        .find(|file| file.path == renamed)
        .unwrap();
    assert_eq!(rename.original_path.as_deref(), Some("content"));
    call(
        &fixture,
        &entry,
        Request::Diff {
            view: DiffView::Staged,
            max_bytes: 100,
        },
    )
    .await
    .unwrap();
    call(&fixture, &entry, Request::Log { limit: 1 })
        .await
        .unwrap();
    assert_eq!(std::fs::read(index).unwrap(), before);
}

#[tokio::test]
async fn tracked_commits_and_explicit_additions_work_in_git_clones_and_native_jj() {
    for backend in ["git", "clone", "jj"] {
        let fixture = if backend == "jj" {
            Fixture::jj()
        } else {
            Fixture::new()
        };
        let request = if backend == "jj" {
            fixture.jj_request()
        } else if backend == "clone" {
            CreateRequest {
                policy: WorkspaceTemplate::Clone { base_ref: None },
                ..fixture.request()
            }
        } else {
            fixture.request()
        };
        let entry = fixture.create(request).await.unwrap();
        let root = Path::new(entry.workspace.location().unwrap().root.as_str());
        std::fs::write(root.join("content"), "tracked work\n").unwrap();
        std::fs::write(root.join("new file [x]"), "explicit addition\n").unwrap();
        std::fs::write(root.join("leave alone"), "untracked\n").unwrap();
        let mut request = commit(Inclusion::TrackedWorkingCopy);
        if let Request::Commit {
            message, untracked, ..
        } = &mut request
        {
            *message = "--help\n\nrecord literal agent message".into();
            untracked.push("new file [x]".into());
        }
        let result = call(&fixture, &entry, request).await.unwrap();
        assert_eq!(result.outcome, "recorded", "{backend}");
        assert!(
            result
                .state
                .files
                .iter()
                .any(|file| file.path == "leave alone" && file.untracked)
        );
        assert!(
            !result
                .state
                .files
                .iter()
                .any(|file| file.path == "new file [x]" && file.untracked)
        );
        let recorded = result.recorded.unwrap();
        assert_eq!(
            recorded.message.trim_end(),
            "--help\n\nrecord literal agent message"
        );
        let history = call(&fixture, &entry, Request::Log { limit: 5 })
            .await
            .unwrap();
        assert!(
            history
                .history
                .iter()
                .any(|change| change.identity.commit_id == recorded.identity.commit_id)
        );
        if backend == "jj" {
            assert!(recorded.identity.change_id.is_some());
            assert_ne!(recorded.identity.commit_id, result.state.identity.commit_id);
        } else {
            assert_eq!(
                git(root, &["show", "HEAD:content"], "test").unwrap(),
                "tracked work\n"
            );
        }
    }
}

#[tokio::test]
async fn jj_status_and_layered_diff_do_not_snapshot_physical_edits() {
    let fixture = Fixture::jj();
    let entry = fixture.create(fixture.jj_request()).await.unwrap();
    let root = Path::new(entry.workspace.location().unwrap().root.as_str());
    let original = call(&fixture, &entry, Request::Status {})
        .await
        .unwrap()
        .state
        .identity;
    std::fs::write(root.join("content"), "physical edit\n").unwrap();
    std::fs::write(root.join("not tracked"), "leave alone\n").unwrap();
    let status = call(&fixture, &entry, Request::Status {}).await.unwrap();
    assert_eq!(status.state.identity.commit_id, original.commit_id);
    assert_eq!(status.state.identity.operation_id, original.operation_id);
    assert!(
        status
            .state
            .files
            .iter()
            .any(|file| file.path == "content" && file.working_copy.is_some())
    );
    let diff = call(
        &fixture,
        &entry,
        Request::Diff {
            view: DiffView::WorkingCopy,
            max_bytes: 65536,
        },
    )
    .await
    .unwrap();
    assert_eq!(diff.diff[0].layer, "recorded_change");
    assert!(diff.diff.iter().any(
        |section| section.layer == "physical_edits" && section.text.contains("+physical edit")
    ));
    assert_eq!(diff.state.identity.commit_id, original.commit_id);
    assert_eq!(diff.state.identity.operation_id, original.operation_id);
}

#[tokio::test]
async fn vcs_empty_changes_stale_tokens_and_explicit_ignored_files_are_handled() {
    for native_jj in [false, true] {
        let fixture = if native_jj {
            Fixture::jj()
        } else {
            Fixture::new()
        };
        let entry = fixture
            .create(if native_jj {
                fixture.jj_request()
            } else {
                fixture.request()
            })
            .await
            .unwrap();
        let root = Path::new(entry.workspace.location().unwrap().root.as_str());
        assert_eq!(
            call(&fixture, &entry, commit(Inclusion::TrackedWorkingCopy))
                .await
                .unwrap_err(),
            api::Error::Empty
        );
        std::fs::write(root.join(".gitignore"), "ignored.txt\n").unwrap();
        std::fs::write(root.join("ignored.txt"), "explicitly ignored\n").unwrap();
        std::fs::write(root.join("content"), "updated\n").unwrap();
        let mut stale = commit(Inclusion::TrackedWorkingCopy);
        if let Request::Commit { expected_state, .. } = &mut stale {
            *expected_state = Some("stale".into());
        }
        assert_eq!(
            call(&fixture, &entry, stale).await.unwrap_err(),
            api::Error::Concurrent
        );
        let first = call(&fixture, &entry, commit(Inclusion::TrackedWorkingCopy))
            .await
            .unwrap();
        assert!(
            first
                .state
                .files
                .iter()
                .any(|file| file.path == "ignored.txt" && file.ignored)
        );
        let mut explicit = commit(Inclusion::TrackedWorkingCopy);
        if let Request::Commit { ignored, .. } = &mut explicit {
            ignored.push("ignored.txt".into());
        }
        let recorded = call(&fixture, &entry, explicit).await.unwrap();
        assert!(recorded.recorded.is_some());
        assert!(
            !recorded
                .state
                .files
                .iter()
                .any(|file| file.path == "ignored.txt")
        );
    }
}

#[tokio::test]
async fn vcs_diff_truncation_does_not_turn_a_large_patch_into_backend_failure() {
    let fixture = Fixture::new();
    let entry = fixture.create(fixture.request()).await.unwrap();
    let root = Path::new(entry.workspace.location().unwrap().root.as_str());
    std::fs::write(
        root.join("content"),
        "a long line of change\n".repeat(100_000),
    )
    .unwrap();
    let result = call(
        &fixture,
        &entry,
        Request::Diff {
            view: DiffView::WorkingCopy,
            max_bytes: 100,
        },
    )
    .await
    .unwrap();
    assert!(result.diff[0].truncated);
    assert!(result.diff[0].text.len() <= 100);
}

#[tokio::test]
async fn vcs_service_rejects_foreign_owners_before_accessing_a_workspace() {
    let fixture = Fixture::new();
    let entry = fixture.create(fixture.request()).await.unwrap();
    fixture
        .service
        .attach(entry.workspace.id().clone(), "vcs-session".into())
        .await
        .unwrap();
    let root = PathBuf::from(entry.workspace.location().unwrap().root.as_str());
    let context = hive_protocol::workspace::Context {
        owner: Some(NodeId::new()),
        project: entry.workspace.intent().project.to_string(),
        workspace: entry.workspace.id().to_string(),
        effective_cwd: root.display().to_string(),
    };
    for request in [
        Request::Status {},
        Request::Diff {
            view: DiffView::WorkingCopy,
            max_bytes: 100,
        },
        commit(Inclusion::TrackedWorkingCopy),
        Request::Log { limit: 1 },
    ] {
        assert!(matches!(
            fixture
                .service
                .tool_execute(
                    "vcs-session".into(),
                    context.clone(),
                    root.join("absent"),
                    Some(request),
                    || Ok(())
                )
                .await,
            Err(hive_protocol::workspace_tools::ToolError::Owner)
        ));
    }
}

#[tokio::test]
async fn an_interrupted_mutation_job_never_becomes_a_retryable_read_failure() {
    let fixture = Fixture::new();
    let entry = fixture.create(fixture.request()).await.unwrap();
    let root = PathBuf::from(entry.workspace.location().unwrap().root.as_str());
    std::fs::write(root.join("content"), "pending mutation\n").unwrap();
    fixture
        .service
        .attach(entry.workspace.id().clone(), "interrupted-vcs".into())
        .await
        .unwrap();
    let context = hive_protocol::workspace::Context {
        owner: Some(entry.workspace.intent().node.clone()),
        project: entry.workspace.intent().project.to_string(),
        workspace: entry.workspace.id().to_string(),
        effective_cwd: root.display().to_string(),
    };
    let checks = std::sync::atomic::AtomicUsize::new(0);
    let result = fixture
        .service
        .tool_execute(
            "interrupted-vcs".into(),
            context,
            root,
            Some(commit(Inclusion::TrackedWorkingCopy)),
            move || {
                // Lose the worker after it crossed an authority fence, not while queued for admission.
                assert!(
                    checks.fetch_add(1, std::sync::atomic::Ordering::AcqRel) < 2,
                    "simulated interrupted mutation worker"
                );
                Ok(())
            },
        )
        .await;
    assert_eq!(
        result.unwrap_err(),
        hive_protocol::workspace_tools::ToolError::Vcs(api::Error::Uncertain)
    );
}

#[tokio::test]
async fn vcs_mutation_fences_reject_revocation_and_external_edits_before_dispatch() {
    for native_jj in [false, true] {
        for revoke in [false, true] {
            let fixture = if native_jj {
                Fixture::jj()
            } else {
                Fixture::new()
            };
            let entry = fixture
                .create(if native_jj {
                    fixture.jj_request()
                } else {
                    fixture.request()
                })
                .await
                .unwrap();
            let root = PathBuf::from(entry.workspace.location().unwrap().root.as_str());
            std::fs::write(root.join("content"), "reviewed\n").unwrap();
            let result = fixture
                .service
                .run(move |rt| {
                    let selection = rt.selection(&entry)?;
                    let repository = selection.repository.as_ref().unwrap();
                    let backend = rt.manager.repository(repository)?;
                    let fence = || {
                        if revoke {
                            return Err(api::Error::Cancelled);
                        }
                        // A barrier at the dispatch fence avoids sleep-dependent race tests.
                        std::fs::write(root.join("content"), "external edit\n").unwrap();
                        Ok(())
                    };
                    Ok(backend.vcs(
                        api::Target {
                            repository,
                            root: &entry.workspace.location().unwrap().root,
                            authorize_mutation: &fence,
                        },
                        &commit(Inclusion::TrackedWorkingCopy),
                    ))
                })
                .await
                .unwrap();
            assert_eq!(
                result.unwrap_err(),
                if revoke {
                    api::Error::Cancelled
                } else {
                    api::Error::Concurrent
                }
            );
        }
    }
}

#[tokio::test]
async fn git_conflicts_never_report_a_successful_commit() {
    let fixture = Fixture::new();
    let entry = fixture.create(fixture.request()).await.unwrap();
    let root = Path::new(entry.workspace.location().unwrap().root.as_str());
    let head = git(root, &["rev-parse", "HEAD"], "test").unwrap();
    // A native merge supplies conflict records with no dependence on the user's merge drivers.
    git(root, &["checkout", "-b", "conflict-side"], "test").unwrap();
    std::fs::write(root.join("content"), "side\n").unwrap();
    git(root, &["commit", "-am", "side"], "test").unwrap();
    git(root, &["checkout", "-"], "test").unwrap();
    std::fs::write(root.join("content"), "ours\n").unwrap();
    git(root, &["commit", "-am", "ours"], "test").unwrap();
    assert!(git(root, &["merge", "conflict-side"], "test").is_err());
    assert!(
        call(&fixture, &entry, Request::Status {})
            .await
            .unwrap()
            .state
            .conflicts
    );
    assert_eq!(
        call(&fixture, &entry, commit(Inclusion::StagedOnly))
            .await
            .unwrap_err(),
        api::Error::Conflict
    );
    git(root, &["merge", "--abort"], "test").unwrap();
    assert_ne!(git(root, &["rev-parse", "HEAD"], "test").unwrap(), head);
}

#[cfg(unix)]
#[tokio::test]
async fn git_hook_failures_report_uncertain_effects_without_suppressing_native_hooks() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    let entry = fixture.create(fixture.request()).await.unwrap();
    let root = Path::new(entry.workspace.location().unwrap().root.as_str());
    let hooks = fixture._directory.path().join("test-hooks");
    std::fs::create_dir(&hooks).unwrap();
    let hook = hooks.join("pre-commit");
    std::fs::write(&hook, "#!/bin/sh\nprintf ran > hook-ran\nexit 1\n").unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o700)).unwrap();
    git(
        root,
        &["config", "core.hooksPath", hooks.to_str().unwrap()],
        "test",
    )
    .unwrap();
    std::fs::write(root.join("content"), "pending\n").unwrap();
    let before = git(root, &["rev-parse", "HEAD"], "test").unwrap();
    assert_eq!(
        call(&fixture, &entry, commit(Inclusion::TrackedWorkingCopy))
            .await
            .unwrap_err(),
        api::Error::Uncertain
    );
    assert!(root.join("hook-ran").is_file());
    assert_eq!(git(root, &["rev-parse", "HEAD"], "test").unwrap(), before);
}

#[cfg(unix)]
#[tokio::test]
async fn jj_inspection_preserves_symlinks_and_detects_modes_binary_data_and_directory_deletion() {
    use std::os::unix::{fs::PermissionsExt, fs::symlink};
    let fixture = Fixture::jj();
    let entry = fixture.create(fixture.jj_request()).await.unwrap();
    let root = Path::new(entry.workspace.location().unwrap().root.as_str());
    std::fs::create_dir(root.join("nested")).unwrap();
    std::fs::write(root.join("nested/file"), "nested\n").unwrap();
    symlink("missing target", root.join("link")).unwrap();
    let mut additions = commit(Inclusion::TrackedWorkingCopy);
    if let Request::Commit { untracked, .. } = &mut additions {
        *untracked = vec!["nested/file".into(), "link".into()];
    }
    call(&fixture, &entry, additions).await.unwrap();
    let before = call(&fixture, &entry, Request::Status {})
        .await
        .unwrap()
        .state
        .identity;
    std::fs::remove_dir_all(root.join("nested")).unwrap();
    std::fs::remove_file(root.join("link")).unwrap();
    symlink("different target", root.join("link")).unwrap();
    std::fs::write(root.join("content"), b"binary\0data").unwrap();
    std::fs::set_permissions(root.join("content"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let status = call(&fixture, &entry, Request::Status {}).await.unwrap();
    assert_eq!(status.state.identity.commit_id, before.commit_id);
    assert!(
        status
            .state
            .files
            .iter()
            .any(|file| file.path == "nested/file" && file.working_copy.as_deref() == Some("D"))
    );
    assert!(
        status
            .state
            .files
            .iter()
            .any(|file| file.path == "link" && file.working_copy.is_some())
    );
    let diff = call(
        &fixture,
        &entry,
        Request::Diff {
            view: DiffView::WorkingCopy,
            max_bytes: 65536,
        },
    )
    .await
    .unwrap();
    assert!(
        diff.diff
            .iter()
            .any(|section| section.text.contains("Binary files differ"))
    );
}

#[tokio::test]
async fn a_worktrunk_selection_dispatches_vcs_through_its_git_repository_only() {
    use hive_core::workspace::backend::ProvisionerId;
    let fixture = Fixture::new();
    let entry = fixture.create(fixture.request()).await.unwrap();
    std::fs::write(
        Path::new(entry.workspace.location().unwrap().root.as_str()).join("content"),
        "Git backend\n",
    )
    .unwrap();
    fixture
        .service
        .run(move |rt| {
            let mut selection = rt.selection(&entry)?;
            selection.provisioner = ProvisionerId("worktrunk".into());
            // VCS dispatch consumes only the pinned repository; no provisioner method is involved.
            let repository = selection.repository.as_ref().unwrap();
            assert_eq!(repository.backend.0, "git");
            let backend = rt.manager.repository(repository)?;
            let result = backend
                .vcs(
                    api::Target {
                        repository,
                        root: &entry.workspace.location().unwrap().root,
                        authorize_mutation: &|| Ok(()),
                    },
                    &commit(Inclusion::TrackedWorkingCopy),
                )
                .unwrap();
            assert!(result.recorded.is_some());
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn jj_conflicted_changes_are_visible_and_cannot_be_recorded_by_the_common_tool() {
    let fixture = Fixture::jj();
    let entry = fixture.create(fixture.jj_request()).await.unwrap();
    let root = Path::new(entry.workspace.location().unwrap().root.as_str());
    std::fs::write(root.join("content"), "left\n").unwrap();
    execute("jj", Some(root), &["commit", "-m", "left"], false).unwrap();
    let left = execute(
        "jj",
        Some(root),
        &[
            "--ignore-working-copy",
            "log",
            "--no-graph",
            "-r",
            "@-",
            "-T",
            "commit_id",
        ],
        false,
    )
    .unwrap();
    execute("jj", Some(root), &["new", "@--"], false).unwrap();
    std::fs::write(root.join("content"), "right\n").unwrap();
    execute("jj", Some(root), &["commit", "-m", "right"], false).unwrap();
    let right = execute(
        "jj",
        Some(root),
        &[
            "--ignore-working-copy",
            "log",
            "--no-graph",
            "-r",
            "@-",
            "-T",
            "commit_id",
        ],
        false,
    )
    .unwrap();
    execute("jj", Some(root), &["new", left.trim(), right.trim()], false).unwrap();
    let status = call(&fixture, &entry, Request::Status {}).await.unwrap();
    assert!(status.state.conflicts);
    assert_eq!(
        call(&fixture, &entry, commit(Inclusion::TrackedWorkingCopy))
            .await
            .unwrap_err(),
        api::Error::Conflict
    );
}
