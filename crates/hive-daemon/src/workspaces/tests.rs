use super::*;

#[path = "tests/recovery.rs"]
mod recovery_tests;

#[path = "tests/backends.rs"]
mod backend_tests;

#[path = "tests/mcp.rs"]
mod mcp_tests;

#[path = "tests/jj.rs"]
mod jj_tests;

#[cfg(unix)]
#[path = "tests/activity.rs"]
mod activity_tests;

#[path = "../../tests/support/fake_opencode.rs"]
mod fake_opencode;

struct Fixture {
    _directory: tempfile::TempDir,
    service: Service,
    remote: PathBuf,
    seed: PathBuf,
    definition: ProjectDefinition,
}

#[tokio::test]
async fn worktrunk_chooses_its_own_path_and_removes_the_checkout_in_the_foreground() {
    if probe_worktrunk() != WorktrunkAvailability::Usable {
        eprintln!("skipping real WorkTrunk smoke test: compatible wt is unavailable");
        return;
    }
    let fixture = Fixture::new();
    let request = fixture.request();
    let native = fixture.create(fixture.request()).await.unwrap();
    let wt_config = fixture._directory.path().join("config/worktrunk");
    std::fs::create_dir_all(&wt_config).unwrap();
    let custom = fixture._directory.path().join("custom-worktrees");
    std::fs::write(
        wt_config.join("config.toml"),
        format!(
            "worktree-path = '{}/{{{{ branch | sanitize }}}}'\n",
            custom.display()
        ),
    )
    .unwrap();
    let entry = fixture
        .service
        .create(
            request.clone(),
            fixture.definition.clone(),
            WorktrunkConfig {
                enabled: Some(true),
            },
            None,
        )
        .await
        .unwrap();
    let location = entry.workspace.location().unwrap();
    assert_eq!(location.backend.0, "worktrunk");
    assert!(Path::new(location.root.as_str()).is_dir());
    assert!(Path::new(location.root.as_str()).starts_with(custom));
    assert_eq!(entry.repository, native.repository);
    assert_eq!(
        entry.selection.as_ref().unwrap().repository,
        native.selection.as_ref().unwrap().repository
    );
    assert_eq!(fixture.service.repositories().await.unwrap().len(), 1);
    let restarted = Service::new(fixture.service.store.clone());
    let replayed = restarted
        .create(
            request,
            fixture.definition.clone(),
            WorktrunkConfig {
                enabled: Some(false),
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(replayed.selection, entry.selection);
    fixture
        .service
        .remove(entry.workspace.id().clone(), OperationId::new(), false)
        .await
        .unwrap();
    assert!(!Path::new(location.root.as_str()).exists());
}

#[tokio::test]
async fn worktrunk_requires_prior_approval_for_branch_specific_project_hooks() {
    if probe_worktrunk() != WorktrunkAvailability::Usable {
        return;
    }
    let fixture = Fixture::new();
    std::fs::create_dir(fixture.seed.join(".config")).unwrap();
    std::fs::write(
        fixture.seed.join(".config/wt.toml"),
        "pre-start = 'touch hook-ran && git commit --allow-empty -m prepared'\n",
    )
    .unwrap();
    git(&fixture.seed, &["add", ".config/wt.toml"], "test").unwrap();
    git(&fixture.seed, &["commit", "-m", "add project hook"], "test").unwrap();
    git(
        &fixture.seed,
        &["push", fixture.remote.to_str().unwrap(), "main"],
        "test",
    )
    .unwrap();
    let request = fixture.request();
    assert!(
        fixture
            .service
            .create(
                request.clone(),
                fixture.definition.clone(),
                WorktrunkConfig {
                    enabled: Some(true)
                },
                None
            )
            .await
            .is_err()
    );
    let key = request.operation.to_string();
    fixture
        .service
        .run(move |rt| {
            let bare = rt
                .state
                .repositories
                .values()
                .next()
                .unwrap()
                .repository
                .root
                .as_str();
            let config = rt
                .store
                .state_directory()
                .join("worktrunk-config")
                .join(format!("{key}.toml"));
            // Only this temporary fixture's harmless hook is approved, before retrying provisioning.
            wt_with_config(
                Path::new(bare),
                &["config", "approvals", "add", "--yes"],
                Some(&config),
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let entry = fixture
        .service
        .create(
            request,
            fixture.definition.clone(),
            WorktrunkConfig {
                enabled: Some(true),
            },
            None,
        )
        .await
        .unwrap();
    assert!(
        Path::new(entry.workspace.location().unwrap().root.as_str())
            .join("hook-ran")
            .is_file()
    );
    assert_ne!(
        entry
            .workspace
            .location()
            .unwrap()
            .resolved_base
            .as_ref()
            .unwrap()
            .as_str(),
        entry.workspace.location().unwrap().commit.as_ref().unwrap()
    );
}

impl Fixture {
    fn jj() -> Self {
        let mut fixture = Self::new();
        fixture.definition.scm = hive_core::project::Scm::Jj;
        fixture.definition.default_revision =
            Some(hive_core::project::Revision::try_from("main@origin".to_owned()).unwrap());
        fixture
    }
    fn jj_request(&self) -> CreateRequest {
        CreateRequest {
            policy: WorkspaceTemplate::Jj { revision: None },
            ..self.request()
        }
    }
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let seed = directory.path().join("seed");
        let remote = directory.path().join("remote.git");
        let config = directory.path().join("gitconfig");
        std::fs::write(directory.path().join("jj.toml"), "[user]\nname = 'Hive Tests'\nemail = 'hive@example.test'\n[signing]\nbehavior = 'drop'\n").unwrap();
        // Isolate even fixture bootstrap from developer signing, hooks, templates and credentials.
        std::fs::write(&config, format!("[url \"{}\"]\n insteadOf = https://example.test/project\n[protocol \"file\"]\n allow = always\n[user]\n name = Hive Tests\n email = hive@example.test\n", reqwest::Url::from_file_path(&remote).unwrap())).unwrap();
        TEST_GIT_CONFIG.with(|current| *current.borrow_mut() = Some(config.clone()));
        std::fs::create_dir(&seed).unwrap();
        git(&seed, &["init", "-b", "main"], "test").unwrap();
        git(&seed, &["config", "user.name", "Hive Tests"], "test").unwrap();
        git(
            &seed,
            &["config", "user.email", "hive@example.test"],
            "test",
        )
        .unwrap();
        std::fs::write(seed.join("content"), "original\n").unwrap();
        git(&seed, &["add", "content"], "test").unwrap();
        git(&seed, &["commit", "-m", "initial"], "test").unwrap();
        git(
            &seed,
            &[
                "clone",
                "--bare",
                seed.to_str().unwrap(),
                remote.to_str().unwrap(),
            ],
            "test",
        )
        .unwrap();
        let store = Arc::new(Store::open(directory.path().join("state/catalog.sqlite3")).unwrap());
        store.save_resource("test_git_config", &config).unwrap();
        let service = Service {
            store,
            manager: Arc::new(backends::manager()),
            git_config: Some(config),
        };
        let definition = ProjectDefinition {
            scm: hive_core::project::Scm::Git,
            default_revision: None,
            name: "Project".into(),
            repository_source: Some("https://example.test/project".into()),
            default_ref: None,
            preparation: vec![],
        };
        Self {
            _directory: directory,
            service,
            remote,
            seed,
            definition,
        }
    }
    fn request(&self) -> CreateRequest {
        CreateRequest {
            revision: None,
            operation: OperationId::new(),
            project: ProjectId::from_name("project"),
            policy: WorkspaceTemplate::Worktree { base_ref: None },
            base_ref: None,
            branch: None,
        }
    }
    async fn create(&self, request: CreateRequest) -> Result<Entry, Error> {
        self.service
            .create(
                request,
                self.definition.clone(),
                WorktrunkConfig {
                    enabled: Some(false),
                },
                None,
            )
            .await
    }
}

#[tokio::test]
async fn simultaneous_workspaces_share_one_flat_bare_repository_without_sharing_branches() {
    let fixture = Fixture::new();
    let (first, second) = tokio::join!(
        fixture.create(fixture.request()),
        fixture.create(fixture.request())
    );
    let first = first.unwrap();
    let second = second.unwrap();
    let a = first.workspace.location().unwrap();
    let b = second.workspace.location().unwrap();
    assert_ne!(a.root, b.root);
    assert_ne!(a.branch, b.branch);
    let repos = fixture.service.repositories().await.unwrap();
    assert_eq!(repos.len(), 1);
    let bare = Path::new(repos[0].repository.root.as_str());
    assert_eq!(bare.file_name().unwrap(), ".git");
    assert_eq!(Path::new(a.root.as_str()).parent(), bare.parent());
    assert_eq!(Path::new(b.root.as_str()).parent(), bare.parent());
    assert_eq!(
        git(bare, &["rev-parse", "--is-bare-repository"], "test")
            .unwrap()
            .trim(),
        "true"
    );
    assert_eq!(
        git(
            Path::new(a.root.as_str()),
            &["rev-parse", "--is-bare-repository"],
            "test"
        )
        .unwrap()
        .trim(),
        "false"
    );
    assert!(git(bare, &["config", "--local", "--get", "core.bare"], "test").is_err());
    assert_eq!(
        git(
            bare,
            &["config", "--get", "extensions.worktreeConfig"],
            "test"
        )
        .unwrap()
        .trim(),
        "true"
    );
}

#[tokio::test]
async fn fetch_advances_new_workspace_bases_without_moving_existing_checkouts() {
    let fixture = Fixture::new();
    let first = fixture.create(fixture.request()).await.unwrap();
    std::fs::write(fixture.seed.join("content"), "new upstream\n").unwrap();
    git(&fixture.seed, &["commit", "-am", "advance"], "test").unwrap();
    git(
        &fixture.seed,
        &["push", fixture.remote.to_str().unwrap(), "main"],
        "test",
    )
    .unwrap();
    let second = fixture.create(fixture.request()).await.unwrap();
    assert_ne!(
        first.workspace.location().unwrap().commit,
        second.workspace.location().unwrap().commit
    );
    assert_eq!(
        std::fs::read_to_string(
            Path::new(first.workspace.location().unwrap().root.as_str()).join("content")
        )
        .unwrap(),
        "original\n"
    );
}

#[tokio::test]
async fn retries_keep_identity_and_changed_intent_conflicts() {
    let fixture = Fixture::new();
    let request = fixture.request();
    let first = fixture.create(request.clone()).await.unwrap();
    let replay = fixture.create(request.clone()).await.unwrap();
    assert_eq!(first.workspace, replay.workspace);
    let mut changed = request;
    changed.branch = Some(GitRef::try_from("other".to_owned()).unwrap());
    assert!(matches!(
        fixture.create(changed).await,
        Err(Error::RetryConflict)
    ));
}

#[tokio::test]
async fn branch_collisions_and_invalid_bases_leave_inspectable_failures() {
    let fixture = Fixture::new();
    let mut request = fixture.request();
    request.branch = Some(GitRef::try_from("task".to_owned()).unwrap());
    let first = fixture.create(request.clone()).await.unwrap();
    request.operation = OperationId::new();
    assert!(matches!(
        fixture.create(request).await,
        Err(Error::Conflict)
    ));
    let mut invalid = fixture.request();
    invalid.base_ref = Some(GitRef::try_from("absent-base".to_owned()).unwrap());
    assert!(fixture.create(invalid).await.is_err());
    assert_eq!(
        fixture
            .service
            .list()
            .await
            .unwrap()
            .iter()
            .filter(|e| e.workspace.state() == WorkspaceState::Failed)
            .count(),
        2
    );
    assert!(matches!(
        fixture
            .service
            .inspect(first.workspace.id().clone())
            .await
            .unwrap(),
        Inspection::Present { .. }
    ));
}

#[tokio::test]
async fn failed_fetch_preserves_workspaces_and_the_same_operation_can_recover() {
    let fixture = Fixture::new();
    let first = fixture.create(fixture.request()).await.unwrap();
    let hidden = fixture.remote.with_extension("offline");
    std::fs::rename(&fixture.remote, &hidden).unwrap();
    let request = fixture.request();
    assert!(matches!(
        fixture.create(request.clone()).await,
        Err(Error::Git("fetch"))
    ));
    assert_eq!(
        fixture.service.repositories().await.unwrap()[0].status,
        "failed"
    );
    std::fs::rename(hidden, &fixture.remote).unwrap();
    fixture.create(request).await.unwrap();
    assert!(matches!(
        fixture
            .service
            .inspect(first.workspace.id().clone())
            .await
            .unwrap(),
        Inspection::Present { .. }
    ));
}

#[tokio::test]
async fn cleanup_protects_dirty_and_active_workspaces_even_with_discard() {
    let fixture = Fixture::new();
    let entry = fixture.create(fixture.request()).await.unwrap();
    let id = entry.workspace.id().clone();
    let path = PathBuf::from(entry.workspace.location().unwrap().root.as_str());
    std::fs::write(path.join("untracked"), "important").unwrap();
    assert!(
        fixture
            .service
            .remove(id.clone(), OperationId::new(), false)
            .await
            .is_err()
    );
    fixture
        .service
        .attach(id.clone(), "session".into())
        .await
        .unwrap();
    assert!(
        fixture
            .service
            .remove(id.clone(), OperationId::new(), true)
            .await
            .is_err()
    );
    fixture.service.release("session".into()).await.unwrap();
    fixture
        .service
        .remove(id, OperationId::new(), true)
        .await
        .unwrap();
    assert!(!path.exists());
    let repos = fixture.service.repositories().await.unwrap();
    git(
        Path::new(repos[0].repository.root.as_str()),
        &[
            "show-ref",
            "--verify",
            &format!(
                "refs/heads/{}",
                entry
                    .workspace
                    .location()
                    .unwrap()
                    .branch
                    .as_ref()
                    .unwrap()
                    .as_str()
            ),
        ],
        "test",
    )
    .unwrap();
}

#[tokio::test]
async fn restart_preserves_use_holds_and_never_recreates_missing_resources() {
    let fixture = Fixture::new();
    let entry = fixture.create(fixture.request()).await.unwrap();
    fixture
        .service
        .attach(entry.workspace.id().clone(), "session".into())
        .await
        .unwrap();
    let reopened = Service {
        store: Arc::new(
            Store::open(
                fixture
                    .service
                    .store
                    .state_directory()
                    .join("catalog.sqlite3"),
            )
            .unwrap(),
        ),
        git_config: fixture.service.git_config.clone(),
        manager: fixture.service.manager.clone(),
    };
    assert_eq!(
        reopened.node().await.unwrap(),
        fixture.service.node().await.unwrap()
    );
    assert!(
        reopened
            .remove(entry.workspace.id().clone(), OperationId::new(), true)
            .await
            .is_err()
    );
    std::fs::rename(
        entry.workspace.location().unwrap().root.as_str(),
        fixture.seed.join("moved"),
    )
    .unwrap();
    assert!(reopened.attached("session".into()).await.is_err());
    assert!(!Path::new(entry.workspace.location().unwrap().root.as_str()).exists());
}

#[tokio::test]
async fn independent_clone_survives_cache_removal_and_existing_directories_are_never_deleted() {
    let fixture = Fixture::new();
    let mut request = fixture.request();
    request.policy = WorkspaceTemplate::Clone { base_ref: None };
    let entry = fixture.create(request).await.unwrap();
    let repos = fixture.service.repositories().await.unwrap();
    std::fs::remove_dir_all(repos[0].repository.root.as_str()).unwrap();
    assert!(matches!(
        fixture
            .service
            .inspect(entry.workspace.id().clone())
            .await
            .unwrap(),
        Inspection::Present { .. }
    ));
    let mut request = fixture.request();
    request.policy = WorkspaceTemplate::Existing {
        directory: Some(fixture.seed.clone()),
    };
    let existing = fixture.create(request).await.unwrap();
    assert!(
        fixture
            .service
            .remove(existing.workspace.id().clone(), OperationId::new(), true)
            .await
            .is_err()
    );
    assert!(fixture.seed.exists());
}

#[cfg(unix)]
#[tokio::test]
async fn worktree_local_hooks_execute_independently_without_changing_the_other_checkout() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    let first = fixture.create(fixture.request()).await.unwrap();
    let second = fixture.create(fixture.request()).await.unwrap();
    for (index, entry) in [first, second].into_iter().enumerate() {
        let path = PathBuf::from(entry.workspace.location().unwrap().root.as_str());
        let hooks = fixture.seed.join(format!("hooks-{index}"));
        std::fs::create_dir(&hooks).unwrap();
        let sentinel = fixture.seed.join(format!("sentinel-{index}"));
        let hook = hooks.join("pre-commit");
        std::fs::write(
            &hook,
            format!(
                "#!/bin/sh\nprintf '%s' '{index}' > '{}'\n",
                sentinel.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        git(
            &path,
            &[
                "config",
                "--worktree",
                "core.hooksPath",
                hooks.to_str().unwrap(),
            ],
            "test",
        )
        .unwrap();
        git(
            &path,
            &[
                "-c",
                "user.name=Hive Tests",
                "-c",
                "user.email=hive@example.test",
                "commit",
                "--allow-empty",
                "-m",
                "hook isolation",
            ],
            "test",
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(sentinel).unwrap(),
            index.to_string()
        );
        if index == 0 {
            assert!(!fixture.seed.join("sentinel-1").exists());
        }
        let origin = git(
            &path,
            &["config", "--show-origin", "--get", "core.hooksPath"],
            "test",
        )
        .unwrap();
        assert!(origin.contains("config.worktree"));
    }
    let repos = fixture.service.repositories().await.unwrap();
    assert!(
        git(
            Path::new(repos[0].repository.root.as_str()),
            &["config", "--local", "--get", "core.hooksPath"],
            "test"
        )
        .is_err()
    );
}

#[tokio::test]
async fn profile_policy_provisions_at_admission_and_explicit_existing_launch_bypasses_it() {
    let fixture = Fixture::new();
    let config: crate::config::Config = serde_json::from_value(serde_json::json!({
        "default_agent":"coder", "projects":{"project":fixture.definition},
        "workspaces":{"task":{"policy":"worktree"}}, "worktrunk":{"enabled":false},
        "agents":{"coder":{"command":"unstarted-agent", "project":"project", "workspace":"task"}}
    }))
    .unwrap();
    let broker = crate::Broker::persistent(
        config.clone(),
        fixture
            .service
            .store
            .state_directory()
            .join("catalog.sqlite3"),
    )
    .unwrap();
    let mut first = hive_protocol::v2::NewSessionRequest::new(fixture.seed.clone());
    let context = broker
        .launch_workspace(
            &mut first,
            None,
            &config.agents["coder"],
            Arc::new(config.clone()),
            BTreeMap::new(),
            "session-one".into(),
        )
        .await
        .unwrap()
        .unwrap();
    assert_ne!(first.cwd.0, fixture.seed);
    assert_eq!(first.cwd.0.to_str().unwrap(), context.effective_cwd);
    let mut second = hive_protocol::v2::NewSessionRequest::new(fixture.seed.clone());
    broker
        .launch_workspace(
            &mut second,
            None,
            &config.agents["coder"],
            Arc::new(config.clone()),
            BTreeMap::new(),
            "session-two".into(),
        )
        .await
        .unwrap();
    assert_ne!(first.cwd, second.cwd);
    let mut bypass = hive_protocol::v2::NewSessionRequest::new(fixture.seed.clone());
    assert!(
        broker
            .launch_workspace(
                &mut bypass,
                Some(hive_protocol::workspace::Launch::Existing {
                    directory: fixture.seed.display().to_string()
                }),
                &config.agents["coder"],
                Arc::new(config.clone()),
                BTreeMap::new(),
                "session-three".into()
            )
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(bypass.cwd.0, fixture.seed);
    let mut legacy_profile = config.agents["coder"].clone();
    legacy_profile.workspace = None;
    let mut legacy = hive_protocol::v2::NewSessionRequest::new(fixture.seed.clone());
    assert!(
        broker
            .launch_workspace(
                &mut legacy,
                None,
                &legacy_profile,
                Arc::new(config),
                BTreeMap::new(),
                "legacy".into()
            )
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(legacy.cwd.0, fixture.seed);
    assert_eq!(fixture.service.list().await.unwrap().len(), 2);
}

#[tokio::test]
async fn the_core_backend_boundary_replays_durable_intent_and_preserves_cleanup_request_identity() {
    use hive_core::workspace::backend::{
        CleanupOutcome, ProvisionOutcome, ProvisionRequest, WorkspaceBackend,
    };
    let fixture = Fixture::new();
    let entry = fixture.create(fixture.request()).await.unwrap();
    let backend: &dyn WorkspaceBackend = &fixture.service;
    let provision = ProvisionRequest {
        operation: entry.workspace.operation().unwrap().clone(),
        workspace: entry.workspace.id().clone(),
        backend: entry.workspace.backend().unwrap().clone(),
        intent: entry.workspace.intent().clone(),
    };
    assert_eq!(
        backend.provision(provision).await.unwrap().outcome,
        ProvisionOutcome::AlreadySatisfied
    );
    let request = CleanupRequest {
        operation: OperationId::new(),
        target: ResourceTarget {
            workspace: entry.workspace.id().clone(),
            node: entry.workspace.intent().node.clone(),
            backend: entry.workspace.backend().unwrap().clone(),
            ownership: Ownership::HiveManaged,
            location: entry.workspace.location().cloned(),
        },
        mode: CleanupMode::PreserveChanges,
    };
    assert_eq!(
        backend.cleanup(request.clone()).await.unwrap(),
        CleanupOutcome::Removed
    );
    assert_eq!(
        backend.cleanup(request.clone()).await.unwrap(),
        CleanupOutcome::AlreadyAbsent
    );
    let mut changed = request;
    changed.target.location = None;
    assert!(matches!(
        backend.cleanup(changed).await,
        Err(hive_core::workspace::WorkspaceError::RetryConflict { .. })
    ));
}

#[tokio::test]
async fn interrupted_clone_acquisition_reuses_its_staging_identity_after_the_remote_recovers() {
    let fixture = Fixture::new();
    let hidden = fixture.remote.with_extension("offline");
    std::fs::rename(&fixture.remote, &hidden).unwrap();
    let request = fixture.request();
    assert!(fixture.create(request.clone()).await.is_err());
    let first = fixture.service.list().await.unwrap().remove(0);
    let repository = fixture.service.repositories().await.unwrap().remove(0);
    assert!(
        Path::new(repository.repository.root.as_str())
            .parent()
            .unwrap()
            .join(".git-incomplete")
            .exists()
    );
    std::fs::rename(hidden, &fixture.remote).unwrap();
    let recovered = fixture.create(request).await.unwrap();
    assert_eq!(first.workspace.id(), recovered.workspace.id());
    assert_eq!(fixture.service.repositories().await.unwrap().len(), 1);
}

#[tokio::test]
async fn removing_an_independent_clean_clone_archives_its_unpushed_branch() {
    let fixture = Fixture::new();
    let mut request = fixture.request();
    request.policy = WorkspaceTemplate::Clone { base_ref: None };
    let entry = fixture.create(request).await.unwrap();
    let path = Path::new(entry.workspace.location().unwrap().root.as_str());
    git(path, &["commit", "--allow-empty", "-m", "unpushed"], "test").unwrap();
    let commit = git(path, &["rev-parse", "HEAD"], "test").unwrap();
    std::fs::write(path.join("stashed"), "preserve stash").unwrap();
    git(path, &["add", "stashed"], "test").unwrap();
    git(path, &["stash", "push", "-m", "saved work"], "test").unwrap();
    let removed = fixture
        .service
        .remove(entry.workspace.id().clone(), OperationId::new(), false)
        .await
        .unwrap();
    let archive = removed.archive.unwrap();
    assert_eq!(
        git(Path::new(archive.as_str()), &["rev-parse", "HEAD"], "test").unwrap(),
        commit
    );
    assert_eq!(
        git(
            Path::new(archive.as_str()),
            &["show", "refs/stash:stashed"],
            "test"
        )
        .unwrap(),
        "preserve stash"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn disabled_worktrunk_never_probes_and_command_failures_do_not_fall_back_or_leak_stderr() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    let executable = fixture._directory.path().join("fake-wt");
    let log = fixture._directory.path().join("wt-calls");
    std::fs::write(&executable, format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\ncase \"$*\" in\n'switch --help') printf '%s' '--format --no-cd --create';;\n'remove --help') printf '%s' '--foreground --no-delete-branch --format';;\n'config approvals list --help') printf '%s' '--format';;\n'config state marker'*'--help') printf '%s' 'get set clear --format --branch';;\n'config approvals list --format=json') printf '%s' '{{\"state\":\"no_commands\"}}';;\n*) printf '%s' 'secret-sentinel' >&2; exit 1;;\nesac\n", log.display())).unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
    fixture.create(fixture.request()).await.unwrap();
    assert!(!log.exists());
    let error = fixture
        .service
        .create(
            fixture.request(),
            fixture.definition.clone(),
            WorktrunkConfig {
                enabled: Some(true),
            },
            None,
        )
        .await
        .unwrap_err();
    assert!(!error.to_string().contains("secret-sentinel"));
    let calls = std::fs::read_to_string(log).unwrap();
    assert!(calls.contains("switch --create --base"));
    assert!(!calls.contains("--path"));
    assert!(!calls.contains("--yes"));
    assert_eq!(
        fixture
            .service
            .list()
            .await
            .unwrap()
            .iter()
            .filter(|entry| entry.workspace.state() == WorkspaceState::Ready)
            .count(),
        1
    );
}

#[tokio::test]
async fn interrupted_native_creation_reconciles_only_its_registered_branch_commit_and_path() {
    let fixture = Fixture::new();
    let entry = fixture.create(fixture.request()).await.unwrap();
    let id = entry.workspace.id().clone();
    let interrupted = id.clone();
    fixture
        .service
        .run(move |rt| {
            let entry = rt.state.workspaces.get_mut(interrupted.as_str()).unwrap();
            let mut workspace = Workspace::new(interrupted, entry.workspace.intent().clone())?;
            workspace.transition(WorkspaceEvent::Provision {
                operation: entry.workspace.operation().unwrap().clone(),
                backend: entry.workspace.backend().unwrap().clone(),
            })?;
            git(
                Path::new(entry.workspace.location().unwrap().root.as_str()),
                &["config", "--worktree", "--unset", "hive.workspace"],
                "test",
            )?;
            entry.workspace = workspace;
            rt.save()
        })
        .await
        .unwrap();
    assert!(matches!(
        fixture.service.inspect(id.clone()).await.unwrap(),
        Inspection::IdentityConflict
    ));
    assert!(
        fixture
            .service
            .reconcile(id.clone(), Some(fixture.seed.clone()), false)
            .await
            .is_err()
    );
    let reconciled = fixture
        .service
        .reconcile(id.clone(), None, false)
        .await
        .unwrap();
    assert_eq!(reconciled.workspace.state(), WorkspaceState::Ready);
    assert_eq!(reconciled.workspace.location(), entry.workspace.location());
    assert_eq!(fixture.service.list().await.unwrap().len(), 1);
}

#[tokio::test]
async fn native_cleanup_reconciles_a_lost_response_without_repeating_removal_or_losing_its_branch()
{
    let fixture = Fixture::new();
    let entry = fixture.create(fixture.request()).await.unwrap();
    let id = entry.workspace.id().clone();
    let operation = OperationId::new();
    let lost = operation.clone();
    let target = id.clone();
    fixture
        .service
        .run(move |rt| {
            let mut entry = rt.state.workspaces.get(target.as_str()).unwrap().clone();
            let location = entry.workspace.location().unwrap().clone();
            let request = CleanupRequest {
                operation: lost.clone(),
                target: ResourceTarget {
                    workspace: target.clone(),
                    node: rt.state.node.clone(),
                    backend: location.backend.clone(),
                    ownership: Ownership::HiveManaged,
                    location: Some(location.clone()),
                },
                mode: CleanupMode::PreserveChanges,
            };
            entry.workspace.transition(WorkspaceEvent::Cleanup {
                operation: lost.clone(),
            })?;
            rt.state.operations.insert(
                lost.to_string(),
                Operation {
                    request: serde_json::to_value(request).unwrap(),
                    workspace: target.clone(),
                    commit: None,
                    path: Some(PathBuf::from(location.root.as_str())),
                    effects_started: true,
                    selection: entry.selection.clone(),
                    kind: Some(OperationKind::Cleanup),
                    prepared: None,
                },
            );
            rt.state
                .workspaces
                .insert(target.to_string(), entry.clone());
            rt.save()?;
            let bare = rt
                .state
                .repositories
                .get(entry.repository.as_ref().unwrap().as_str())
                .unwrap()
                .repository
                .root
                .as_str();
            git(
                Path::new(bare),
                &["worktree", "remove", location.root.as_str()],
                "test",
            )?;
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(
        fixture
            .service
            .remove(id, operation, false)
            .await
            .unwrap()
            .workspace
            .state(),
        WorkspaceState::Removed
    );
}

#[tokio::test]
async fn resource_routes_expose_durable_operations_and_reject_foreign_or_dirty_removal() {
    let fixture = Fixture::new();
    let config: crate::config::Config = serde_json::from_value(serde_json::json!({"projects":{"project":fixture.definition}, "workspaces":{"task":{"policy":"worktree"}},"worktrunk":{"enabled":false}})).unwrap();
    let broker = crate::Broker::persistent(
        config,
        fixture
            .service
            .store
            .state_directory()
            .join("catalog.sqlite3"),
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let router = crate::router_with_origin(broker, Some(&url));
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let client = reqwest::Client::new();
    let operation = OperationId::new();
    let sweep = serde_json::json!({"operation":OperationId::new(),"workspaces":[]});
    assert_eq!(
        client
            .post(format!("{url}/v1/workspace-sweeps"))
            .header("origin", "https://foreign.test")
            .json(&sweep)
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::FORBIDDEN
    );
    client
        .post(format!("{url}/v1/workspace-sweeps"))
        .json(&sweep)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let sweep_id = sweep["operation"].as_str().unwrap();
    assert_eq!(
        client
            .post(format!("{url}/v1/workspace-sweeps/{sweep_id}/apply"))
            .header("origin", "https://foreign.test")
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::FORBIDDEN
    );
    assert_eq!(
        client
            .post(format!("{url}/v1/workspace-sweeps/{sweep_id}/apply"))
            .json(&serde_json::json!({"discard_changes":true}))
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::UNPROCESSABLE_ENTITY
    );
    client
        .get(format!("{url}/v1/workspaces/recovery"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let request = serde_json::json!({"operation":operation,"project":"project","policy":"task"});
    assert_eq!(
        client
            .post(format!("{url}/v1/workspaces"))
            .header("origin", "https://foreign.test")
            .json(&request)
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::FORBIDDEN
    );
    let response = client
        .post(format!("{url}/v1/workspaces"))
        .json(&request)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let entry: Entry = response.json().await.unwrap();
    let id = entry.workspace.id();
    let replay: Entry = client
        .post(format!("{url}/v1/workspaces"))
        .json(&request)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(id, replay.workspace.id());
    let observed: serde_json::Value = client
        .get(format!("{url}/v1/operations/{operation}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(observed["workspace"], id.as_str());
    assert!(observed["commit"].is_string());
    std::fs::write(
        Path::new(entry.workspace.location().unwrap().root.as_str()).join("untracked"),
        "keep",
    )
    .unwrap();
    let remove = serde_json::json!({"operation":OperationId::new(),"discard_changes":false});
    let rejected = client
        .post(format!("{url}/v1/workspaces/{id}/remove"))
        .json(&remove)
        .send()
        .await
        .unwrap();
    assert_eq!(rejected.status(), reqwest::StatusCode::CONFLICT);
    let error: serde_json::Value = rejected.json().await.unwrap();
    assert_eq!(error["code"], "hive::workspace::unsafe_cleanup");
    client
        .post(format!("{url}/v1/workspaces/{id}/remove"))
        .json(&serde_json::json!({"operation":OperationId::new(),"discard_changes":true}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert!(!Path::new(entry.workspace.location().unwrap().root.as_str()).exists());
    server.abort();
}

#[tokio::test]
async fn abandoned_pre_dispatch_reservations_can_be_released_but_live_or_uncertain_dispatch_cannot()
{
    let fixture = Fixture::new();
    let first = fixture.create(fixture.request()).await.unwrap();
    fixture
        .service
        .attach(first.workspace.id().clone(), "startup".into())
        .await
        .unwrap();
    let broker = crate::Broker::persistent(
        crate::config::Config::default(),
        fixture
            .service
            .store
            .state_directory()
            .join("catalog.sqlite3"),
    )
    .unwrap();
    broker.registry.lock().unwrap().bindings.insert(
        "startup".into(),
        crate::reload::Binding {
            profile: "unused".into(),
            kind: crate::config::ProfileKind::Acp,
            identity: None,
        },
    );
    assert!(
        broker
            .remove_workspace(first.workspace.id().clone(), OperationId::new(), false)
            .await
            .is_err()
    );
    broker.registry.lock().unwrap().bindings.remove("startup");
    broker
        .remove_workspace(first.workspace.id().clone(), OperationId::new(), false)
        .await
        .unwrap();
    let second = fixture.create(fixture.request()).await.unwrap();
    fixture
        .service
        .attach(second.workspace.id().clone(), "uncertain".into())
        .await
        .unwrap();
    fixture
        .service
        .mark_dispatched("uncertain".into())
        .await
        .unwrap();
    assert!(
        broker
            .remove_workspace(second.workspace.id().clone(), OperationId::new(), true)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn provisioning_tracks_remote_default_branch_changes_and_accepts_explicit_tag_bases() {
    let fixture = Fixture::new();
    git(&fixture.seed, &["tag", "initial"], "test").unwrap();
    git(
        &fixture.seed,
        &["push", fixture.remote.to_str().unwrap(), "--tags"],
        "test",
    )
    .unwrap();
    let first = fixture.create(fixture.request()).await.unwrap();
    git(&fixture.seed, &["checkout", "-b", "trunk"], "test").unwrap();
    std::fs::write(fixture.seed.join("content"), "new default").unwrap();
    git(&fixture.seed, &["commit", "-am", "new default"], "test").unwrap();
    git(
        &fixture.seed,
        &["push", fixture.remote.to_str().unwrap(), "trunk"],
        "test",
    )
    .unwrap();
    git(
        &fixture.remote,
        &["symbolic-ref", "HEAD", "refs/heads/trunk"],
        "test",
    )
    .unwrap();
    let second = fixture.create(fixture.request()).await.unwrap();
    assert_ne!(
        first.workspace.location().unwrap().commit,
        second.workspace.location().unwrap().commit
    );
    let mut tagged = fixture.request();
    tagged.base_ref = Some(GitRef::try_from("refs/tags/initial".to_owned()).unwrap());
    let tagged = fixture.create(tagged).await.unwrap();
    assert_eq!(
        first.workspace.location().unwrap().commit,
        tagged.workspace.location().unwrap().commit
    );
}

#[tokio::test]
async fn invalid_existing_directories_do_not_reserve_unrecoverable_operation_keys() {
    let fixture = Fixture::new();
    let mut request = fixture.request();
    request.policy = WorkspaceTemplate::Existing {
        directory: Some(fixture.seed.join("missing")),
    };
    assert!(fixture.create(request.clone()).await.is_err());
    assert!(matches!(
        fixture.service.operation(request.operation.clone()).await,
        Err(Error::Unknown)
    ));
    std::fs::create_dir(fixture.seed.join("missing")).unwrap();
    assert_eq!(
        fixture.create(request).await.unwrap().workspace.state(),
        WorkspaceState::Ready
    );
}

#[cfg(unix)]
#[tokio::test]
async fn legacy_cwd_aliases_keep_their_scope_while_acquiring_verified_workspace_use() {
    let fixture = Fixture::new();
    let entry = fixture.create(fixture.request()).await.unwrap();
    let alias = fixture.seed.join("alias");
    std::os::unix::fs::symlink(entry.workspace.location().unwrap().root.as_str(), &alias).unwrap();
    let context = fixture
        .service
        .track_directory(alias.clone(), "legacy".into())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(context.effective_cwd, alias.to_str().unwrap());
    assert_eq!(context.workspace, entry.workspace.id().as_str());
    assert!(
        fixture
            .service
            .remove(entry.workspace.id().clone(), OperationId::new(), true)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn existing_aliases_protect_every_managed_checkout_their_sessions_use_after_restart() {
    let fixture = Fixture::new();
    let managed = fixture.create(fixture.request()).await.unwrap();
    let path = PathBuf::from(managed.workspace.location().unwrap().root.as_str());
    let mut request = fixture.request();
    request.policy = WorkspaceTemplate::Existing {
        directory: Some(path.clone()),
    };
    let alias = fixture.create(request.clone()).await.unwrap();
    fixture
        .service
        .attach(alias.workspace.id().clone(), "attached".into())
        .await
        .unwrap();
    let reopened = Service::new(Arc::new(
        Store::open(
            fixture
                .service
                .store
                .state_directory()
                .join("catalog.sqlite3"),
        )
        .unwrap(),
    ));
    assert!(
        reopened
            .remove(managed.workspace.id().clone(), OperationId::new(), true)
            .await
            .is_err()
    );
    assert_eq!(
        reopened
            .attached("attached".into())
            .await
            .unwrap()
            .unwrap()
            .workspace
            .id(),
        alias.workspace.id()
    );
    reopened.release("attached".into()).await.unwrap();
    request.operation = OperationId::new();
    reopened
        .create(
            request,
            fixture.definition.clone(),
            WorktrunkConfig {
                enabled: Some(false),
            },
            Some("created".into()),
        )
        .await
        .unwrap();
    assert!(
        reopened
            .remove(managed.workspace.id().clone(), OperationId::new(), true)
            .await
            .is_err()
    );
    reopened.release("created".into()).await.unwrap();
    reopened
        .remove(managed.workspace.id().clone(), OperationId::new(), false)
        .await
        .unwrap();
    assert!(!path.exists());
}

#[tokio::test]
async fn initial_operation_and_workspace_publication_is_atomic_even_when_acquisition_fails() {
    let fixture = Fixture::new();
    let db = rusqlite::Connection::open(
        fixture
            .service
            .store
            .state_directory()
            .join("catalog.sqlite3"),
    )
    .unwrap();
    // Reject every durable snapshot with an operation pointing to an absent workspace,
    // rather than testing only the final state after a successful creation.
    db.execute_batch("CREATE TRIGGER operation_requires_workspace BEFORE UPDATE OF record ON resources WHEN NEW.key='node' BEGIN SELECT CASE WHEN EXISTS (SELECT 1 FROM json_each(NEW.record, '$.operations') operation LEFT JOIN json_each(NEW.record, '$.workspaces') workspace ON workspace.key=json_extract(operation.value,'$.workspace') WHERE workspace.key IS NULL) THEN RAISE(ABORT,'orphan workspace operation') END; END;").unwrap();
    let request = fixture.request();
    let hidden = fixture.remote.with_extension("offline");
    std::fs::rename(&fixture.remote, &hidden).unwrap();
    assert!(matches!(
        fixture.create(request.clone()).await,
        Err(Error::Git("clone"))
    ));
    let failed = fixture.service.list().await.unwrap().remove(0);
    std::fs::rename(hidden, &fixture.remote).unwrap();
    assert_eq!(
        fixture.create(request).await.unwrap().workspace.id(),
        failed.workspace.id()
    );
}

#[tokio::test]
async fn interrupted_archive_copies_and_publication_reconcile_without_losing_the_source() {
    for (partial, published) in [(true, false), (false, false), (false, true)] {
        let fixture = Fixture::new();
        let mut request = fixture.request();
        request.policy = WorkspaceTemplate::Clone { base_ref: None };
        let entry = fixture.create(request).await.unwrap();
        let id = entry.workspace.id().clone();
        let operation = OperationId::new();
        let interrupted = operation.clone();
        let target = id.clone();
        fixture
            .service
            .run(move |rt| {
                let mut entry = rt.state.workspaces.get(target.as_str()).unwrap().clone();
                let location = entry.workspace.location().unwrap().clone();
                let source = Path::new(location.root.as_str());
                let request = CleanupRequest {
                    operation: interrupted.clone(),
                    target: ResourceTarget {
                        workspace: target.clone(),
                        node: rt.state.node.clone(),
                        backend: location.backend.clone(),
                        ownership: Ownership::HiveManaged,
                        location: Some(location.clone()),
                    },
                    mode: CleanupMode::PreserveChanges,
                };
                entry.workspace.transition(WorkspaceEvent::Cleanup {
                    operation: interrupted.clone(),
                })?;
                let directory = rt.store.state_directory().join("archives");
                let plan = ArchivePlan {
                    destination: directory.join(format!("{target}.git")),
                    staging: directory.join(format!(".{target}-{interrupted}.incomplete")),
                    head: git(source, &["rev-parse", "HEAD"], "test")?,
                    refs: git(
                        source,
                        &["for-each-ref", "--format=%(objectname) %(refname)"],
                        "test",
                    )?,
                };
                rt.state
                    .archives
                    .insert(interrupted.to_string(), plan.clone());
                rt.state.operations.insert(
                    interrupted.to_string(),
                    Operation {
                        request: serde_json::to_value(request).unwrap(),
                        workspace: target.clone(),
                        commit: None,
                        path: Some(source.to_owned()),
                        effects_started: true,
                        selection: entry.selection.clone(),
                        kind: Some(OperationKind::Cleanup),
                        prepared: None,
                    },
                );
                rt.state.workspaces.insert(target.to_string(), entry);
                rt.save()?;
                std::fs::create_dir_all(&directory).unwrap();
                if partial {
                    git(
                        &directory,
                        &["init", "--bare", plan.staging.to_str().unwrap()],
                        "test",
                    )?;
                } else {
                    git(
                        source,
                        &[
                            "clone",
                            "--mirror",
                            "--no-hardlinks",
                            source.to_str().unwrap(),
                            plan.staging.to_str().unwrap(),
                        ],
                        "test",
                    )?;
                }
                if published {
                    git(
                        &plan.staging,
                        &["config", "hive.archiveOperation", interrupted.as_str()],
                        "test",
                    )?;
                    std::fs::rename(&plan.staging, &plan.destination).unwrap();
                }
                // Simulate process death before the completed archive association is saved.
                Ok(())
            })
            .await
            .unwrap();
        let reopened = Service::new(Arc::new(
            Store::open(
                fixture
                    .service
                    .store
                    .state_directory()
                    .join("catalog.sqlite3"),
            )
            .unwrap(),
        ));
        let removed = reopened.remove(id, operation, false).await.unwrap();
        assert_eq!(removed.workspace.state(), WorkspaceState::Removed);
        assert!(Path::new(removed.archive.unwrap().as_str()).is_dir());
        assert!(!Path::new(entry.workspace.location().unwrap().root.as_str()).exists());
    }
}

#[tokio::test]
async fn adopted_native_sessions_hold_their_managed_directory_without_provisioning_again() {
    use agent_client_protocol::{
        Client,
        schema::{ProtocolVersion, v2},
    };
    let fixture = Fixture::new();
    let entry = fixture.create(fixture.request()).await.unwrap();
    let path = PathBuf::from(entry.workspace.location().unwrap().root.as_str());
    let (native, url) = fake_opencode::start(fake_opencode::Options::default()).await;
    native.add_session("ses_managed", "Managed", path.to_str().unwrap());
    let config: crate::config::Config = serde_json::from_value(serde_json::json!({"default_agent":"oc", "projects":{"project":fixture.definition}, "workspaces":{"task":{"policy":"worktree"}}, "worktrunk":{"enabled":false}, "agents":{"oc":{"kind":"opencode-server","endpoint":url,"project":"project","workspace":"task"}}})).unwrap();
    let broker = crate::Broker::persistent(
        config,
        fixture
            .service
            .store
            .state_directory()
            .join("catalog.sqlite3"),
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = crate::router(broker.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let target = entry.workspace.id().clone();
    Client
        .v2()
        .connect_with(
            hive_protocol::HttpClient::new(format!("ws://{address}")).unwrap(),
            async |cx| {
                cx.send_request(v2::InitializeRequest::new(
                    ProtocolVersion::V2,
                    v2::Implementation::new("adoption-holds", "1"),
                ))
                .block_task()
                .await?;
                let mut meta = v2::Meta::new();
                meta.insert(
                    "hive".into(),
                    serde_json::json!({"agent":"oc","adopt":{"nativeId":"ses_managed"}}),
                );
                let opened = cx
                    .send_request(v2::NewSessionRequest::new(path).meta(meta))
                    .block_task()
                    .await?;
                assert_eq!(
                    opened.meta.as_ref().unwrap()["hive"]["workspace"]["workspace"],
                    target.as_str()
                );
                assert!(
                    broker
                        .remove_workspace(target.clone(), OperationId::new(), true)
                        .await
                        .is_err()
                );
                assert_eq!(fixture.service.list().await.unwrap().len(), 1);
                assert_eq!(native.exact("POST /session"), 0);
                cx.send_request(v2::CloseSessionRequest::new(opened.session_id).meta(opened.meta))
                    .block_task()
                    .await?;
                Ok(())
            },
        )
        .await
        .unwrap();
    broker
        .remove_workspace(target, OperationId::new(), false)
        .await
        .unwrap();
    broker.shutdown().await;
    server.abort();
}

#[tokio::test]
async fn unmanaged_remote_provider_directories_need_not_exist_on_the_gateway() {
    use agent_client_protocol::{
        Client,
        schema::{ProtocolVersion, v2},
    };
    let fixture = Fixture::new();
    let (_, url) = fake_opencode::start(fake_opencode::Options::default()).await;
    let config: crate::config::Config = serde_json::from_value(serde_json::json!({"default_agent":"oc","agents":{"oc":{"kind":"opencode-server","endpoint":url}}})).unwrap();
    let broker = crate::Broker::persistent(
        config,
        fixture
            .service
            .store
            .state_directory()
            .join("catalog.sqlite3"),
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = crate::router(broker.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let path = fixture.seed.join("only-on-the-provider");
    assert!(!path.exists());
    Client
        .v2()
        .connect_with(
            hive_protocol::HttpClient::new(format!("ws://{address}")).unwrap(),
            async |cx| {
                cx.send_request(v2::InitializeRequest::new(
                    ProtocolVersion::V2,
                    v2::Implementation::new("remote-cwd", "1"),
                ))
                .block_task()
                .await?;
                let opened = cx
                    .send_request(v2::NewSessionRequest::new(path))
                    .block_task()
                    .await?;
                assert!(opened.meta.as_ref().unwrap()["hive"]["workspace"].is_null());
                cx.send_request(v2::CloseSessionRequest::new(opened.session_id).meta(opened.meta))
                    .block_task()
                    .await?;
                Ok(())
            },
        )
        .await
        .unwrap();
    broker.shutdown().await;
    server.abort();
}

#[tokio::test]
async fn cancelled_persisted_startups_remain_visible_and_keep_their_dispatched_workspace_hold() {
    let fixture = Fixture::new();
    let entry = fixture.create(fixture.request()).await.unwrap();
    let broker = crate::Broker::persistent(
        crate::config::Config::default(),
        fixture
            .service
            .store
            .state_directory()
            .join("catalog.sqlite3"),
    )
    .unwrap();
    fixture
        .service
        .attach(entry.workspace.id().clone(), "pending".into())
        .await
        .unwrap();
    fixture
        .service
        .mark_dispatched("pending".into())
        .await
        .unwrap();
    let record: crate::persistence::Record = serde_json::from_value(serde_json::json!({"id":"pending", "downstream_id":"", "cwd":entry.workspace.location().unwrap().root.as_str(), "profile":"unused", "version":"v2", "created_at":1, "next_seq":1, "reserved_seq":0, "generation":1, "outcome":"recovering", "uncertain":false})).unwrap();
    broker.update_offline(record.clone());
    let pending = crate::PendingStartup {
        broker: broker.clone(),
        record: Some(Arc::new(std::sync::Mutex::new(record))),
        armed: true,
        evidence: Arc::default(),
    };
    let (started, running) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let _pending = pending;
        let _ = started.send(());
        std::future::pending::<()>().await;
    });
    running.await.unwrap();
    task.abort();
    let _ = task.await;
    assert!(matches!(
        broker.offline.lock().unwrap()["pending"].outcome,
        crate::persistence::Outcome::Unavailable
    ));
    assert_eq!(
        serde_json::to_value(broker.snapshot(None).unwrap()).unwrap()["sessions"][0]["lifecycle"],
        "unavailable"
    );
    assert!(matches!(
        fixture.service.store.records().unwrap()[0].outcome,
        crate::persistence::Outcome::Unavailable
    ));
    assert!(
        broker
            .remove_workspace(entry.workspace.id().clone(), OperationId::new(), true)
            .await
            .is_err()
    );
}

#[cfg(unix)]
#[test]
fn command_timeouts_stop_descendants_even_after_the_leader_exits_and_join_all_readers() {
    for wait in ["wait", "exit"] {
        let directory = tempfile::tempdir().unwrap();
        let log = directory.path().join("descendant");
        let mut command = Command::new("sh");
        command.args(["-c", "sh -c 'while :; do printf x >> \"$1\"; sleep 0.02; done' worker \"$1\" & if [ \"$2\" = wait ]; then wait; fi", "leader", log.to_str().unwrap(), wait]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let started = std::time::Instant::now();
        assert!(matches!(
            capture_command(command, Duration::from_millis(250), false),
            Err(Error::Uncertain)
        ));
        assert!(started.elapsed() < Duration::from_secs(3));
        let size = std::fs::metadata(&log).unwrap().len();
        assert!(size > 0);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(
            std::fs::metadata(log).unwrap().len(),
            size,
            "descendant survived timeout after {wait}"
        );
    }
}

#[test]
fn windows_git_arguments_use_portable_paths_without_changing_refs_or_remotes() {
    assert_eq!(
        git_path_argument(r"\\?\C:\Users\runner\project"),
        "C:/Users/runner/project"
    );
    assert_eq!(
        git_path_argument(r"\\?\UNC\server\share\project"),
        "//server/share/project"
    );
    assert_eq!(
        git_path_argument(r"C:\Users\runner\project"),
        "C:/Users/runner/project"
    );
    for value in [
        "refs/heads/main",
        "git@example.test:project",
        "https://example.test/project",
        "--verify",
    ] {
        assert_eq!(git_path_argument(value), value);
    }
}

#[cfg(unix)]
#[tokio::test]
async fn state_directory_aliases_and_missing_worktree_registrations_resolve_physical_paths() {
    let fixture = Fixture::new();
    let alias = fixture.seed.join("state-alias");
    std::os::unix::fs::symlink(fixture.service.store.state_directory(), &alias).unwrap();
    let reopened = Service::new(Arc::new(
        Store::open(alias.join("catalog.sqlite3")).unwrap(),
    ));
    assert_eq!(
        reopened.store.state_directory(),
        fixture.service.store.state_directory()
    );
    let missing = fixture.service.store.state_directory().join("missing");
    let list = format!(
        "worktree {}\0branch refs/heads/task\0\0",
        alias.join("missing").display()
    );
    assert!(registered_worktree(&list, &missing).unwrap());
    assert!(!registered_worktree(&list, &missing.with_file_name("different")).unwrap());
}

#[tokio::test]
async fn long_project_identifiers_do_not_expand_managed_repository_or_worktree_paths() {
    let fixture = Fixture::new();
    let mut request = fixture.request();
    request.project = ProjectId::try_from("project".repeat(22)).unwrap();
    let entry = fixture.create(request).await.unwrap();
    let location = entry.workspace.location().unwrap();
    let root = Path::new(location.root.as_str());
    assert_eq!(root.file_name().unwrap(), entry.workspace.id().as_str());
    assert_eq!(
        root.parent().unwrap().file_name().unwrap(),
        entry.repository.as_ref().unwrap().as_str()
    );
    assert!(Path::new(location.root.as_str()).join("content").is_file());
    fixture
        .service
        .remove(entry.workspace.id().clone(), OperationId::new(), false)
        .await
        .unwrap();
    assert!(!Path::new(location.root.as_str()).exists());
}

async fn workspace_api(broker: crate::Broker) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let router = crate::router_with_origin(broker, Some(&url));
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (url, server)
}

#[tokio::test]
async fn administration_rejects_unknown_resources_malformed_requests_and_missing_persistence_without_mutation()
 {
    let fixture = Fixture::new();
    let config: crate::config::Config = serde_json::from_value(serde_json::json!({"projects":{"project":fixture.definition},"workspaces":{"task":{"policy":"worktree"}},"worktrunk":{"enabled":false}})).unwrap();
    let broker = crate::Broker::persistent(
        config,
        fixture
            .service
            .store
            .state_directory()
            .join("catalog.sqlite3"),
    )
    .unwrap();
    let (url, server) = workspace_api(broker.clone()).await;
    let client = reqwest::Client::new();
    let workspace = WorkspaceId::new();
    let repository = RepositoryId::new();
    for (method, path, expected) in [
        (
            reqwest::Method::GET,
            format!("/v1/workspaces/{workspace}"),
            404,
        ),
        (
            reqwest::Method::GET,
            format!("/v1/workspaces/{workspace}/inspect"),
            404,
        ),
        (
            reqwest::Method::GET,
            format!("/v1/repositories/{repository}"),
            404,
        ),
        (
            reqwest::Method::POST,
            format!("/v1/repositories/{repository}/fetch"),
            404,
        ),
        (reqwest::Method::GET, "/v1/workspaces/invalid".into(), 422),
        (
            reqwest::Method::GET,
            "/v1/workspaces/invalid/inspect".into(),
            422,
        ),
        (reqwest::Method::GET, "/v1/repositories/invalid".into(), 422),
        (
            reqwest::Method::POST,
            "/v1/repositories/invalid/fetch".into(),
            422,
        ),
        (reqwest::Method::GET, "/v1/operations/invalid".into(), 422),
    ] {
        let response = client
            .request(method, format!("{url}{path}"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), expected, "{path}");
        let diagnostic: serde_json::Value = response.json().await.unwrap();
        assert!(
            diagnostic["code"]
                .as_str()
                .unwrap()
                .starts_with("hive::workspace::")
        );
    }
    let request =
        serde_json::json!({"operation":OperationId::new(),"project":"project","policy":"task"});
    for (route, body) in [
        (
            "/v1/workspaces".to_owned(),
            serde_json::json!({"operation":"private-sentinel"}),
        ),
        (
            format!("/v1/workspaces/{workspace}/remove"),
            serde_json::json!({"operation":"private-sentinel"}),
        ),
        (
            format!("/v1/workspaces/{workspace}/reconcile"),
            serde_json::json!({"unknown":"private-sentinel"}),
        ),
    ] {
        let response = client
            .post(format!("{url}{route}"))
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 422);
        assert!(!response.text().await.unwrap().contains("private-sentinel"));
    }
    let mut unknown = request.clone();
    unknown["project"] = serde_json::json!("missing");
    assert_eq!(
        client
            .post(format!("{url}/v1/workspaces"))
            .json(&unknown)
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
        404
    );
    unknown["policy"] = serde_json::json!("missing");
    assert_eq!(
        client
            .post(format!("{url}/v1/workspaces"))
            .json(&unknown)
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
        422
    );
    let mut collision = request.clone();
    collision["branch"] = serde_json::json!("main");
    let response = client
        .post(format!("{url}/v1/workspaces"))
        .json(&collision)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 409);
    assert_eq!(
        response.json::<serde_json::Value>().await.unwrap()["operation"],
        collision["operation"]
    );
    broker.shutdown().await;
    assert_eq!(
        client
            .post(format!("{url}/v1/workspaces"))
            .json(&request)
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
        503
    );
    server.abort();
    let (url, server) = workspace_api(crate::Broker::new(crate::config::Config::default())).await;
    for route in ["/v1/workspaces", "/v1/repositories"] {
        assert_eq!(
            client
                .get(format!("{url}{route}"))
                .send()
                .await
                .unwrap()
                .status()
                .as_u16(),
            503
        );
    }
    server.abort();
}

#[tokio::test]
async fn ephemeral_backings_and_node_directory_overrides_preserve_their_ownership_and_retention_contracts()
 {
    let fixture = Fixture::new();
    for backing in ["worktree", "clone"] {
        let mut request = fixture.request();
        request.policy = WorkspaceTemplate::Ephemeral {
            backing: backing.into(),
            base_ref: Some(GitRef::try_from("main".to_owned()).unwrap()),
            retention: Retention::Until { expires_at: 1 },
        };
        let entry = fixture.create(request).await.unwrap();
        assert!(entry.workspace.intent().retention.eligible_at(2));
        assert_eq!(entry.workspace.state(), WorkspaceState::Ready);
        assert!(matches!(
            entry.workspace.intent().policy,
            WorkspacePolicy::Ephemeral { .. }
        ));
        assert!(matches!(
            fixture
                .service
                .inspect(entry.workspace.id().clone())
                .await
                .unwrap(),
            Inspection::Present { .. }
        ));
        fixture
            .service
            .remove(entry.workspace.id().clone(), OperationId::new(), false)
            .await
            .unwrap();
    }
    let project = ProjectId::from_name("project");
    let node = fixture.service.node().await.unwrap();
    let directory = NodePath::new(
        node.clone(),
        if cfg!(windows) {
            PathSyntax::Windows
        } else {
            PathSyntax::Posix
        },
        fixture.seed.to_str().unwrap().into(),
    )
    .unwrap();
    fixture
        .service
        .store
        .write_project_override(&project, &node, Some(&directory))
        .unwrap();
    let mut request = fixture.request();
    request.policy = WorkspaceTemplate::Existing { directory: None };
    let entry = fixture.create(request).await.unwrap();
    assert_eq!(
        Path::new(entry.workspace.location().unwrap().root.as_str()),
        fixture.seed.canonicalize().unwrap()
    );
    assert!(
        fixture
            .service
            .remove(entry.workspace.id().clone(), OperationId::new(), true)
            .await
            .is_err()
    );
    for policy in [
        serde_json::json!({"policy":"existing","directory":"relative"}),
        serde_json::json!({"policy":"ephemeral","backing":"unsupported","retention":{"kind":"until","expires_at":1}}),
        serde_json::json!({"policy":"ephemeral","backing":"clone","retention":{"kind":"retain"}}),
    ] {
        let config: crate::config::Config =
            serde_json::from_value(serde_json::json!({"workspaces":{"invalid":policy}})).unwrap();
        assert!(config.validate().is_err());
    }
}

#[tokio::test]
async fn core_backend_rejects_foreign_routing_changed_intent_and_cleanup_without_a_matching_managed_identity()
 {
    use hive_core::workspace::{
        WorkspaceError,
        backend::{ProvisionFailure, ProvisionRequest, WorkspaceBackend},
    };
    let fixture = Fixture::new();
    let entry = fixture.create(fixture.request()).await.unwrap();
    let backend: &dyn WorkspaceBackend = &fixture.service;
    let request = ProvisionRequest {
        operation: entry.workspace.operation().unwrap().clone(),
        workspace: entry.workspace.id().clone(),
        backend: entry.workspace.backend().unwrap().clone(),
        intent: entry.workspace.intent().clone(),
    };
    let target = ResourceTarget {
        workspace: entry.workspace.id().clone(),
        node: entry.workspace.intent().node.clone(),
        backend: entry.workspace.backend().unwrap().clone(),
        ownership: Ownership::HiveManaged,
        location: entry.workspace.location().cloned(),
    };
    assert!(matches!(
        backend.inspect(target.clone()).await.unwrap(),
        Inspection::Present { .. }
    ));
    let mut foreign = target.clone();
    foreign.node = NodeId::new();
    assert!(matches!(
        backend.inspect(foreign).await,
        Err(WorkspaceError::NodeMismatch { .. })
    ));
    let mut conflict = target.clone();
    conflict.backend = BackendId("foreign".into());
    assert!(matches!(
        backend.inspect(conflict.clone()).await,
        Err(WorkspaceError::UnsafeCleanup { .. })
    ));
    conflict.location = None;
    assert!(matches!(
        backend.inspect(conflict.clone()).await.unwrap(),
        Inspection::IdentityConflict
    ));
    assert!(matches!(
        backend
            .cleanup(CleanupRequest {
                operation: OperationId::new(),
                target: conflict,
                mode: CleanupMode::DiscardChanges
            })
            .await,
        Err(WorkspaceError::Cleanup { .. })
    ));
    let mut changed = request.clone();
    changed.backend = BackendId("foreign".into());
    assert!(matches!(
        backend.provision(changed).await,
        Err(WorkspaceError::RetryConflict { .. })
    ));
    let mut unknown = target.clone();
    unknown.workspace = WorkspaceId::new();
    assert!(matches!(
        backend.inspect(unknown).await,
        Err(WorkspaceError::InvalidRecord)
    ));
    assert!(matches!(
        backend
            .cleanup(CleanupRequest {
                operation: request.operation.clone(),
                target,
                mode: CleanupMode::DiscardChanges
            })
            .await,
        Err(WorkspaceError::RetryConflict { .. })
    ));
    let mut collision = fixture.request();
    collision.branch = Some(GitRef::try_from("main".to_owned()).unwrap());
    assert!(fixture.create(collision.clone()).await.is_err());
    let failed = fixture
        .service
        .list()
        .await
        .unwrap()
        .into_iter()
        .find(|entry| entry.workspace.operation() == Some(&collision.operation))
        .unwrap();
    assert!(matches!(
        backend
            .provision(ProvisionRequest {
                operation: collision.operation,
                workspace: failed.workspace.id().clone(),
                backend: failed.workspace.backend().unwrap().clone(),
                intent: failed.workspace.intent().clone()
            })
            .await,
        Err(WorkspaceError::Provisioning {
            reason: ProvisionFailure::BranchConflict,
            ..
        })
    ));
    let source = entry.workspace.location().unwrap().root.as_str();
    let moved = fixture.seed.join("moved-managed");
    std::fs::rename(source, &moved).unwrap();
    assert!(matches!(
        backend.provision(request).await,
        Err(WorkspaceError::UncertainOperation { .. })
    ));
    assert!(moved.join("content").exists());
    assert!(!Path::new(source).exists());
}

#[tokio::test]
async fn core_backend_retries_a_transient_acquisition_failure_with_its_original_workspace_and_operation()
 {
    use hive_core::workspace::{
        WorkspaceError,
        backend::{ProvisionOutcome, ProvisionRequest, WorkspaceBackend},
    };
    let fixture = Fixture::new();
    let hidden = fixture.remote.with_extension("offline");
    std::fs::rename(&fixture.remote, &hidden).unwrap();
    let request = fixture.request();
    assert!(fixture.create(request.clone()).await.is_err());
    let failed = fixture.service.list().await.unwrap().remove(0);
    let original = ProvisionRequest {
        operation: request.operation,
        workspace: failed.workspace.id().clone(),
        backend: failed.workspace.backend().unwrap().clone(),
        intent: failed.workspace.intent().clone(),
    };
    let backend: &dyn WorkspaceBackend = &fixture.service;
    assert!(matches!(
        backend.provision(original.clone()).await,
        Err(WorkspaceError::Provisioning {
            retry: RetryDisposition::InspectFirst,
            ..
        })
    ));
    std::fs::rename(hidden, &fixture.remote).unwrap();
    let recovered = backend.provision(original).await.unwrap();
    assert_eq!(&recovered.workspace, failed.workspace.id());
    assert_eq!(recovered.outcome, ProvisionOutcome::Created);
}

#[cfg(unix)]
#[tokio::test]
async fn lost_worktrunk_responses_require_hook_reconciliation_without_repeating_creation_or_removal()
 {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    let executable = fixture._directory.path().join("fake-wt");
    std::fs::write(
        &executable,
        r#"#!/bin/sh
case "$*" in
  'switch --help') printf '%s' '--format --no-cd --create';;
  'remove --help') printf '%s' '--foreground --no-delete-branch --format';;
  'config approvals list --help') printf '%s' '--format';;
  'config state marker'*'--help') printf '%s' 'get set clear --format --branch';;
  'config approvals list --format=json') printf '%s' '{"state":"no_commands"}';;
  switch\ --create*)
    for last do :; done
    git worktree add -b "$last" "$PWD/../hook-checkout" "$4" >/dev/null || exit 1
    printf 'create\n' >> "$HOME/hook-calls"
    printf '{'
    ;;
  remove\ *)
    for last do :; done
    git worktree remove "$last" || exit 1
    printf 'remove\n' >> "$HOME/hook-calls"
    exit 1
    ;;
  *) exit 1;;
esac
"#,
    )
    .unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut request = fixture.request();
    request.policy = WorkspaceTemplate::Ephemeral {
        backing: "worktree".into(),
        base_ref: None,
        retention: Retention::Until { expires_at: 1 },
    };
    assert!(matches!(
        fixture
            .service
            .create(
                request.clone(),
                fixture.definition.clone(),
                WorktrunkConfig {
                    enabled: Some(true)
                },
                None
            )
            .await,
        Err(Error::Uncertain)
    ));
    let failed = fixture.service.list().await.unwrap().remove(0);
    fixture
        .service
        .run(|rt| rt.recover_resources())
        .await
        .unwrap();
    assert_eq!(
        fixture.service.list().await.unwrap()[0].workspace.state(),
        WorkspaceState::Failed
    );
    let repository = fixture.service.repositories().await.unwrap().remove(0);
    let actual = Path::new(repository.repository.root.as_str())
        .parent()
        .unwrap()
        .join("hook-checkout");
    let broker = crate::Broker::persistent(
        crate::config::Config::default(),
        fixture
            .service
            .store
            .state_directory()
            .join("catalog.sqlite3"),
    )
    .unwrap();
    let (url, server) = workspace_api(broker).await;
    let client = reqwest::Client::new();
    let route = format!("{url}/v1/workspaces/{}/reconcile", failed.workspace.id());
    assert_eq!(
        client
            .post(&route)
            .json(&serde_json::json!({"directory":actual}))
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
        409
    );
    let ready: Entry = client
        .post(&route)
        .json(&serde_json::json!({"directory":actual,"hooks_reconciled":true}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(ready.workspace.state(), WorkspaceState::Ready);
    assert_eq!(
        fixture
            .service
            .create(
                request,
                fixture.definition.clone(),
                WorktrunkConfig {
                    enabled: Some(false)
                },
                None
            )
            .await
            .unwrap()
            .workspace
            .id(),
        ready.workspace.id()
    );
    let sweep = fixture
        .service
        .plan_sweep(SweepRequest {
            operation: OperationId::new(),
            workspaces: None,
        })
        .await
        .unwrap();
    let sweep = serde_json::to_value(sweep).unwrap();
    let operation =
        OperationId::try_from(sweep["request"]["operation"].as_str().unwrap().to_owned()).unwrap();
    let applied = serde_json::to_value(
        fixture
            .service
            .apply_sweep(operation.clone())
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(applied["items"][0]["status"], "retryable_failure");
    assert!(!actual.exists());
    fixture
        .service
        .run(|rt| rt.recover_resources())
        .await
        .unwrap();
    let uncertain = serde_json::to_value(
        fixture
            .service
            .apply_sweep(operation.clone())
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(uncertain["items"][0]["status"], "manual_intervention");
    let removed: Entry = client
        .post(route)
        .json(&serde_json::json!({"hooks_reconciled":true}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(removed.workspace.state(), WorkspaceState::Removed);
    let completed =
        serde_json::to_value(fixture.service.apply_sweep(operation).await.unwrap()).unwrap();
    assert_eq!(completed["items"][0]["status"], "already_removed");
    assert_eq!(
        std::fs::read_to_string(fixture._directory.path().join("hook-calls")).unwrap(),
        "create\nremove\n"
    );
    server.abort();
}
