use super::*;
use hive_core::workspace::{WorkspaceError, backend::*};

fn legacy_snapshot(mut value: serde_json::Value) -> serde_json::Value {
    value.as_object_mut().unwrap().remove("version");
    for repository in value["repositories"].as_object_mut().unwrap().values_mut() {
        repository.as_object_mut().unwrap().remove("handle");
    }
    for entry in value["workspaces"].as_object_mut().unwrap().values_mut() {
        entry.as_object_mut().unwrap().remove("selection");
        let policy = &mut entry["workspace"]["intent"]["policy"];
        let backing = if policy["policy"] == "managed" {
            Some(policy.clone())
        } else if policy["policy"] == "ephemeral" {
            Some(policy["backing"].clone())
        } else {
            None
        };
        if let Some(backing) = backing {
            let clone = backing["selection"]["provisioner"] == "git.clone";
            let mut old = serde_json::json!({"kind":if clone { "clone" } else { "worktree" },"git":backing["intent"]["data"]});
            if !clone {
                old["repository"] = backing["selection"]["repository"]["id"].clone();
            }
            if policy["policy"] == "ephemeral" {
                policy["backing"] = old;
            } else {
                old["policy"] = old["kind"].take();
                old.as_object_mut().unwrap().remove("kind");
                *policy = old;
            }
        }
    }
    for operation in value["operations"].as_object_mut().unwrap().values_mut() {
        for key in ["selection", "kind", "prepared"] {
            operation.as_object_mut().unwrap().remove(key);
        }
    }
    fn remove_observations(value: &mut serde_json::Value) {
        match value {
            serde_json::Value::Object(fields) => {
                fields.remove("observations");
                for value in fields.values_mut() {
                    remove_observations(value);
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    remove_observations(item);
                }
            }
            _ => {}
        }
    }
    remove_observations(&mut value);
    value
}

#[tokio::test]
async fn legacy_records_upgrade_atomically_without_rewriting_requests_paths_or_session_holds() {
    let fixture = Fixture::new();
    let request = fixture.request();
    let first = fixture.create(request.clone()).await.unwrap();
    let mut clone_request = fixture.request();
    clone_request.policy = WorkspaceTemplate::Clone { base_ref: None };
    fixture.create(clone_request).await.unwrap();
    fixture
        .service
        .attach(first.workspace.id().clone(), "held".into())
        .await
        .unwrap();
    let original: serde_json::Value = fixture
        .service
        .run(|rt| Ok(serde_json::to_value(&rt.state).unwrap()))
        .await
        .unwrap();
    let legacy = legacy_snapshot(original.clone());
    let requests = legacy["operations"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(id, op)| (id.clone(), op["request"].clone()))
        .collect::<BTreeMap<_, _>>();
    fixture
        .service
        .store
        .save_resource("node", &legacy)
        .unwrap();
    let restarted = Service::new(fixture.service.store.clone());
    let entries = restarted.list().await.unwrap();
    assert_eq!(entries.len(), 2);
    for entry in &entries {
        let selection = entry.selection.as_ref().unwrap();
        assert_eq!(selection.repository.as_ref().unwrap().backend.0, "git");
        assert!(matches!(
            selection.provisioner.0.as_str(),
            "git.worktree" | "git.clone"
        ));
        assert_eq!(
            serde_json::to_value(entry.workspace.location().unwrap()).unwrap()["root"],
            original["workspaces"][entry.workspace.id().as_str()]["workspace"]["location"]["root"]
        );
    }
    let after: serde_json::Value = restarted.store.resource("node").unwrap().unwrap();
    assert_eq!(after["version"], 1);
    assert_eq!(after["uses"], legacy["uses"]);
    for (id, request) in requests {
        assert_eq!(after["operations"][&id]["request"], request);
    }
    // An enabled integration after restart cannot replace the persisted native provisioner.
    let retry = restarted
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
    assert_eq!(retry.workspace.id(), first.workspace.id());
    assert_eq!(retry.selection, first.selection);
    assert!(
        restarted
            .remove(first.workspace.id().clone(), OperationId::new(), true)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn migrated_sweep_requests_keep_their_frozen_cleanup_intent_and_conflict_checks() {
    let fixture = Fixture::new();
    let mut request = fixture.request();
    request.policy = WorkspaceTemplate::Ephemeral {
        backing: "worktree".into(),
        base_ref: None,
        retention: Retention::Until { expires_at: 1 },
    };
    let entry = fixture.create(request).await.unwrap();
    let sweep_operation = OperationId::new();
    fixture
        .service
        .plan_sweep(SweepRequest {
            operation: sweep_operation.clone(),
            workspaces: Some(vec![entry.workspace.id().clone()]),
        })
        .await
        .unwrap();
    let original = fixture
        .service
        .run(|rt| Ok(serde_json::to_value(&rt.state).unwrap()))
        .await
        .unwrap();
    let legacy = legacy_snapshot(original);
    let frozen = legacy["sweeps"].clone();
    fixture
        .service
        .store
        .save_resource("node", &legacy)
        .unwrap();
    fixture.service.list().await.unwrap();
    let after: serde_json::Value = fixture.service.store.resource("node").unwrap().unwrap();
    assert_eq!(after["sweeps"], frozen);
    assert!(matches!(
        fixture
            .service
            .plan_sweep(SweepRequest {
                operation: sweep_operation.clone(),
                workspaces: Some(vec![])
            })
            .await,
        Err(Error::RetryConflict)
    ));
    let mut changed: CleanupRequest =
        serde_json::from_value(frozen[sweep_operation.as_str()]["items"][0]["cleanup"].clone())
            .unwrap();
    changed.target.location.as_mut().unwrap().commit = Some("changed-snapshot".into());
    assert!(matches!(
        fixture.service.cleanup(changed).await,
        Err(WorkspaceError::RetryConflict { .. })
    ));
    fixture.service.apply_sweep(sweep_operation).await.unwrap();
    assert_eq!(
        fixture.service.list().await.unwrap()[0].workspace.state(),
        WorkspaceState::Removed
    );
}

#[tokio::test]
async fn migration_preserves_partial_acquisition_and_settles_interrupted_native_creation() {
    let fixture = Fixture::new();
    let request = fixture.request();
    let offline = fixture.remote.with_extension("offline");
    std::fs::rename(&fixture.remote, &offline).unwrap();
    assert!(fixture.create(request.clone()).await.is_err());
    let snapshot: serde_json::Value = fixture.service.store.resource("node").unwrap().unwrap();
    let id = fixture.service.list().await.unwrap()[0]
        .workspace
        .id()
        .clone();
    fixture
        .service
        .store
        .save_resource("node", &legacy_snapshot(snapshot))
        .unwrap();
    std::fs::rename(offline, &fixture.remote).unwrap();
    let created = fixture.create(request.clone()).await.unwrap();
    assert_eq!(created.workspace.id(), &id);
    let mut snapshot: serde_json::Value = fixture.service.store.resource("node").unwrap().unwrap();
    // Reproduce death after effects and their observed path, but before readiness publication.
    snapshot["workspaces"][id.as_str()]["workspace"]["state"] = "provisioning".into();
    snapshot["workspaces"][id.as_str()]["workspace"]["location"] = serde_json::Value::Null;
    fixture
        .service
        .store
        .save_resource("node", &legacy_snapshot(snapshot))
        .unwrap();
    fixture
        .service
        .run(|rt| rt.recover_resources())
        .await
        .unwrap();
    let recovered = fixture.create(request).await.unwrap();
    assert_eq!(recovered.workspace.id(), &id);
    assert_eq!(recovered.selection, created.selection);
    assert_eq!(
        recovered.workspace.location().unwrap().root,
        created.workspace.location().unwrap().root
    );
}

#[tokio::test]
async fn explicit_bases_work_on_initial_acquisition_with_unborn_or_detached_remote_heads() {
    for detached in [false, true] {
        let fixture = Fixture::new();
        if detached {
            let commit = git(&fixture.remote, &["rev-parse", "main"], "test").unwrap();
            git(
                &fixture.remote,
                &["update-ref", "--no-deref", "HEAD", commit.trim()],
                "test",
            )
            .unwrap();
        } else {
            git(
                &fixture.remote,
                &["symbolic-ref", "HEAD", "refs/heads/missing-default"],
                "test",
            )
            .unwrap();
        }
        let mut request = fixture.request();
        request.base_ref = Some(GitRef::try_from("main".to_owned()).unwrap());
        let created = fixture.create(request).await.unwrap();
        assert_eq!(created.workspace.state(), WorkspaceState::Ready);
        assert_eq!(
            std::fs::read_to_string(
                Path::new(created.workspace.location().unwrap().root.as_str()).join("content")
            )
            .unwrap(),
            "original\n"
        );
    }
}

#[tokio::test]
async fn a_migrated_lost_worktrunk_response_binds_only_an_explicitly_reconciled_observed_path() {
    let fixture = Fixture::new();
    let request = fixture.request();
    let created = fixture.create(request.clone()).await.unwrap();
    let id = created.workspace.id().clone();
    let actual = PathBuf::from(created.workspace.location().unwrap().root.as_str());
    let snapshot: serde_json::Value = fixture.service.store.resource("node").unwrap().unwrap();
    let mut legacy = legacy_snapshot(snapshot);
    let workspace = &mut legacy["workspaces"][id.as_str()]["workspace"];
    workspace["backend"] = "worktrunk".into();
    workspace["state"] = "failed".into();
    workspace["retry"] = "inspect_first".into();
    workspace["location"] = serde_json::Value::Null;
    legacy["operations"][request.operation.as_str()]["path"] = serde_json::Value::Null;
    fixture
        .service
        .store
        .save_resource("node", &legacy)
        .unwrap();
    let restarted = Service::new(fixture.service.store.clone());
    restarted.list().await.unwrap();
    let migrated: serde_json::Value = restarted.store.resource("node").unwrap().unwrap();
    assert_eq!(
        migrated["operations"][request.operation.as_str()]["prepared"]["schema"],
        "hive.git.pending-checkout"
    );
    assert!(matches!(
        restarted
            .reconcile(id.clone(), Some(actual.clone()), false)
            .await,
        Err(Error::Uncertain)
    ));
    assert!(
        restarted
            .reconcile(id.clone(), Some(fixture.seed.clone()), true)
            .await
            .is_err()
    );
    let reconciled = restarted
        .reconcile(id.clone(), Some(actual.clone()), true)
        .await
        .unwrap();
    assert_eq!(reconciled.workspace.state(), WorkspaceState::Ready);
    assert_eq!(
        Path::new(reconciled.workspace.location().unwrap().root.as_str()),
        actual
    );
    assert!(matches!(
        restarted.inspect(id).await.unwrap(),
        Inspection::Present { .. }
    ));
    let replayed = restarted
        .create(
            request.clone(),
            fixture.definition.clone(),
            WorktrunkConfig {
                enabled: Some(false),
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(replayed.selection, reconciled.selection);
    let saved: serde_json::Value = restarted.store.resource("node").unwrap().unwrap();
    assert_eq!(
        saved["operations"][request.operation.as_str()]["request"],
        legacy["operations"][request.operation.as_str()]["request"]
    );
    assert_eq!(
        saved["operations"][request.operation.as_str()]["prepared"]["schema"],
        "hive.git.checkout"
    );
}

#[test]
fn worktrunk_rejects_registered_non_git_storage_before_a_command_can_run() {
    let mut manager = backends::manager();
    // A registered foreign SCM is different from an unavailable implementation.
    manager
        .register_repository(
            RepositoryCompatibility {
                backend: RepositoryBackendId("other".into()),
                storage: StorageKind("other.store".into()),
            },
            Arc::new(backends::GitRepository),
        )
        .unwrap();
    let selection = BackendSelection {
        provisioner: ProvisionerId("worktrunk".into()),
        repository: Some(RepositoryHandle {
            id: RepositoryId::new(),
            project: ProjectId::new(),
            node: NodeId::new(),
            backend: RepositoryBackendId("other".into()),
            storage: StorageKind("other.store".into()),
            metadata: backends::data("other.repository", &()).unwrap(),
        }),
    };
    assert!(matches!(
        manager.provisioner(&selection),
        Err(Error::Domain(WorkspaceError::IncompatibleBackends { .. }))
    ));
}

#[tokio::test]
async fn unknown_provenance_and_future_state_versions_never_fall_back_to_git() {
    let fixture = Fixture::new();
    let entry = fixture.create(fixture.request()).await.unwrap();
    let mut snapshot: serde_json::Value = fixture.service.store.resource("node").unwrap().unwrap();
    snapshot["workspaces"][entry.workspace.id().as_str()]["selection"]["provisioner"] =
        "missing".into();
    fixture
        .service
        .store
        .save_resource("node", &snapshot)
        .unwrap();
    assert!(matches!(
        fixture.service.inspect(entry.workspace.id().clone()).await,
        Err(Error::Domain(WorkspaceError::UnknownProvisioner { .. }))
    ));
    assert!(Path::new(entry.workspace.location().unwrap().root.as_str()).is_dir());
    snapshot["version"] = 99.into();
    fixture
        .service
        .store
        .save_resource("node", &snapshot)
        .unwrap();
    assert!(matches!(
        fixture.service.list().await,
        Err(Error::Domain(WorkspaceError::UnsupportedBackendData { .. }))
    ));
}

struct NonGit {
    workspace: WorkspaceId,
    path: NodePath,
}

impl RepositoryBackend for NonGit {
    type Error = Error;
    fn verify(&self, repository: &RepositoryHandle) -> Result<(), Error> {
        let root: NodePath = repository.metadata.decode("test.store")?;
        if Path::new(root.as_str()).is_dir() {
            Ok(())
        } else {
            Err(Error::Conflict)
        }
    }
    fn acquire(&self, repository: &RepositoryHandle) -> Result<(), Error> {
        let root: NodePath = repository.metadata.decode("test.store")?;
        std::fs::create_dir_all(root.as_str()).map_err(|_| Error::Storage)
    }
    fn update(&self, repository: &RepositoryHandle) -> Result<(), Error> {
        self.verify(repository)
    }
    fn inspect_revision(
        &self,
        _: &RepositoryHandle,
        intent: &BackendData,
    ) -> Result<BackendData, Error> {
        Ok(intent.clone())
    }
}

impl WorkspaceProvisioner for NonGit {
    type Error = Error;
    fn descriptor(&self) -> ProvisionerDescriptor {
        ProvisionerDescriptor {
            id: ProvisionerId("test.workspace".into()),
            repositories: vec![RepositoryCompatibility {
                backend: RepositoryBackendId("test.scm".into()),
                storage: StorageKind("test.store".into()),
            }],
            repository_optional: false,
            capabilities: ProvisionerCapabilities {
                independent_checkout: true,
                ..Default::default()
            },
        }
    }
    fn create(
        &self,
        repository: Option<&RepositoryHandle>,
        intent: &BackendData,
        checkpoints: &mut dyn CheckpointWriter<Error = Error>,
    ) -> Result<WorkspaceObservation, Error> {
        std::fs::create_dir(self.path.as_str()).map_err(|_| Error::Storage)?;
        std::fs::write(
            Path::new(self.path.as_str()).join(".owner"),
            self.workspace.as_str(),
        )
        .map_err(|_| Error::Storage)?;
        checkpoints.checkpoint(backends::data(
            "hive.workspace.path",
            &PathBuf::from(self.path.as_str()),
        )?)?;
        self.inspect(repository, intent)
    }
    fn inspect(
        &self,
        _: Option<&RepositoryHandle>,
        _: &BackendData,
    ) -> Result<WorkspaceObservation, Error> {
        let root = Path::new(self.path.as_str());
        if std::fs::read_to_string(root.join(".owner")).map_err(|_| Error::Conflict)?
            != self.workspace.as_str()
        {
            return Err(Error::Conflict);
        }
        let dirty = std::fs::read_dir(root)
            .map_err(|_| Error::Storage)?
            .any(|entry| entry.unwrap().file_name() != ".owner");
        Ok(WorkspaceObservation {
            root: self.path.clone(),
            working_directory: self.path.clone(),
            metadata: backends::data("test.observed", &())?,
            cleanliness: if dirty {
                Cleanliness::Dirty
            } else {
                Cleanliness::Clean
            },
            ownership_verified: true,
        })
    }
    fn remove(&self, _: Option<&RepositoryHandle>, _: &BackendData, _: bool) -> Result<(), Error> {
        std::fs::remove_dir_all(self.path.as_str()).map_err(|_| Error::Removal)
    }
    fn is_absent(&self, _: Option<&RepositoryHandle>, _: &BackendData) -> Result<bool, Error> {
        Ok(Path::new(self.path.as_str())
            .symlink_metadata()
            .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound))
    }
}

#[tokio::test]
async fn shared_non_git_lifecycle_protects_partial_and_ready_aliases_across_restart_and_cleanup() {
    let directory = tempfile::tempdir().unwrap();
    // Alias admission uses physical paths, including Windows prefixes and macOS /private resolution.
    let root_directory = directory.path().canonicalize().unwrap();
    let store = Arc::new(Store::open(root_directory.join("state/catalog.sqlite3")).unwrap());
    let mut service = Service::new(store.clone());
    let node = service.node().await.unwrap();
    let workspace = WorkspaceId::new();
    let path = NodePath::new(
        node.clone(),
        if cfg!(windows) {
            PathSyntax::Windows
        } else {
            PathSyntax::Posix
        },
        root_directory.join("checkout").to_str().unwrap().into(),
    )
    .unwrap();
    let backend = Arc::new(NonGit {
        workspace: workspace.clone(),
        path: path.clone(),
    });
    let mut manager = BackendManager::default();
    manager
        .register_repository(
            RepositoryCompatibility {
                backend: RepositoryBackendId("test.scm".into()),
                storage: StorageKind("test.store".into()),
            },
            backend.clone(),
        )
        .unwrap();
    manager.register_provisioner(backend).unwrap();
    manager
        .register_provisioner(Arc::new(backends::ExternalDirectory))
        .unwrap();
    service.manager = Arc::new(manager);
    let root_path = root_directory.join("store");
    let root = NodePath::new(
        node.clone(),
        path.syntax(),
        root_path.to_str().unwrap().into(),
    )
    .unwrap();
    let repository = RepositoryHandle {
        id: RepositoryId::new(),
        project: ProjectId::new(),
        node: node.clone(),
        backend: RepositoryBackendId("test.scm".into()),
        storage: StorageKind("test.store".into()),
        metadata: backends::data("test.store", &root).unwrap(),
    };
    let id = workspace.clone();
    let entry = service
        .run(move |rt| {
            let selection = BackendSelection {
                provisioner: ProvisionerId("test.workspace".into()),
                repository: Some(repository.clone()),
            };
            let operation = OperationId::new();
            let mut workspace = Workspace::new(
                id.clone(),
                WorkspaceIntent {
                    project: repository.project.clone(),
                    node,
                    policy: WorkspacePolicy::Managed {
                        selection: Box::new(selection.clone()),
                        intent: backends::data("test.change", &"change-1")?,
                    },
                    retention: Retention::Retain,
                },
            )?;
            workspace.transition(WorkspaceEvent::Provision {
                operation: operation.clone(),
                backend: BackendId(selection.provisioner.0.clone()),
            })?;
            rt.state.repositories.insert(
                repository.id.to_string(),
                Repository {
                    repository: NodeRepository {
                        id: repository.id.clone(),
                        project: repository.project.clone(),
                        root,
                        role: RepositoryRole::Cache,
                        ownership: Ownership::HiveManaged,
                    },
                    source: String::new(),
                    status: "registered".into(),
                    failure: None,
                    handle: Some(repository.clone()),
                },
            );
            let mut entry = Entry {
                workspace,
                repository: Some(repository.id),
                failure: None,
                archive: None,
                selection: Some(selection.clone()),
            };
            rt.state.operations.insert(
                operation.to_string(),
                Operation {
                    request: serde_json::json!({"change":"change-1"}),
                    workspace: id.clone(),
                    commit: None,
                    path: None,
                    effects_started: false,
                    selection: Some(selection),
                    kind: Some(OperationKind::Create),
                    prepared: None,
                },
            );
            rt.state.workspaces.insert(id.to_string(), entry.clone());
            rt.save()?;
            let location = rt.provision(&entry, operation.as_str())?;
            entry
                .workspace
                .transition(WorkspaceEvent::Provisioned { location })?;
            rt.state.workspaces.insert(id.to_string(), entry.clone());
            rt.save()?;
            Ok(entry)
        })
        .await
        .unwrap();
    assert!(entry.workspace.location().unwrap().branch.is_none());
    let partial = workspace.clone();
    service
        .run(move |rt| {
            let entry = rt.state.workspaces.get_mut(partial.as_str()).unwrap();
            let mut record = serde_json::to_value(&entry.workspace).unwrap();
            record["state"] = "provisioning".into();
            record["location"] = serde_json::Value::Null;
            entry.workspace = serde_json::from_value(record).unwrap();
            rt.save()
        })
        .await
        .unwrap();
    let alias = service
        .create(
            CreateRequest {
                revision: None,
                operation: OperationId::new(),
                project: entry.workspace.intent().project.clone(),
                policy: WorkspaceTemplate::Existing {
                    directory: Some(PathBuf::from(path.as_str())),
                },
                base_ref: None,
                branch: None,
            },
            ProjectDefinition {
                scm: hive_core::project::Scm::Git,
                default_revision: None,
                name: "Non-Git alias".into(),
                repository_source: None,
                default_ref: None,
                preparation: vec![],
            },
            WorktrunkConfig {
                enabled: Some(false),
            },
            None,
        )
        .await
        .unwrap();
    // The immutable non-Git create request has no `project` key; the explicit kind must protect it.
    assert!(matches!(
        service
            .attach(alias.workspace.id().clone(), "alias-session".into())
            .await,
        Err(Error::Conflict)
    ));
    service.run(|rt| rt.recover_resources()).await.unwrap();
    service
        .attach(alias.workspace.id().clone(), "alias-session".into())
        .await
        .unwrap();
    let mut reopened = Service::new(store);
    reopened.manager = service.manager.clone();
    assert!(matches!(
        reopened.inspect(workspace.clone()).await.unwrap(),
        Inspection::Present { .. }
    ));
    assert!(matches!(
        reopened
            .remove(workspace.clone(), OperationId::new(), true)
            .await,
        Err(Error::Domain(WorkspaceError::UnsafeCleanup { .. }))
    ));
    assert!(Path::new(path.as_str()).is_dir());
    reopened.release("alias-session".into()).await.unwrap();
    std::fs::write(Path::new(path.as_str()).join("untracked"), "work").unwrap();
    assert!(
        reopened
            .remove(workspace.clone(), OperationId::new(), false)
            .await
            .is_err()
    );
    let removed = reopened
        .remove(workspace, OperationId::new(), true)
        .await
        .unwrap();
    assert_eq!(removed.workspace.state(), WorkspaceState::Removed);
    assert!(!Path::new(path.as_str()).exists());
}
