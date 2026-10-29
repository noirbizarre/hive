use super::*;
use backend::*;

fn path(node: &NodeId, value: &str) -> NodePath {
    NodePath::new(node.clone(), PathSyntax::Posix, value.to_owned()).unwrap()
}

fn intent() -> WorkspaceIntent {
    WorkspaceIntent {
        project: ProjectId::new(),
        node: NodeId::new(),
        policy: WorkspacePolicy::Worktree {
            repository: RepositoryId::new(),
            git: GitIntent {
                base_ref: GitRef::try_from("main".to_owned()).unwrap(),
                branch: None,
            },
        },
        retention: Retention::Retain,
    }
}

fn location(node: &NodeId) -> WorkspaceLocation {
    WorkspaceLocation {
        root: path(node, "/actual/integration/location"),
        working_directory: path(node, "/actual/integration/location/package"),
        backend: BackendId("fake-integration-v1".to_owned()),
        resolved_base: Some(GitRef::try_from("refs/heads/main".to_owned()).unwrap()),
        branch: Some(GitRef::try_from("task/one".to_owned()).unwrap()),
        commit: Some("opaque-object-id".to_owned()),
    }
}

fn workspace_in(state: WorkspaceState) -> Workspace {
    let intent = intent();
    let observed = location(&intent.node);
    Workspace::try_from(WorkspaceRecord {
        id: WorkspaceId::new(),
        intent,
        state,
        operation: (state != WorkspaceState::Requested).then(OperationId::new),
        backend: (state != WorkspaceState::Requested).then(|| observed.backend.clone()),
        retry: matches!(
            state,
            WorkspaceState::Failed | WorkspaceState::CleanupFailed
        )
        .then_some(RetryDisposition::SameOperation),
        location: matches!(
            state,
            WorkspaceState::Ready
                | WorkspaceState::Cleaning
                | WorkspaceState::CleanupFailed
                | WorkspaceState::Removed
        )
        .then_some(observed),
    })
    .unwrap()
}

#[test]
fn every_state_event_pair_obeys_the_documented_transition_table_without_partial_mutation() {
    use WorkspaceState as S;
    // Columns: provision, provisioned, provision failed, cleanup, cleaned,
    // cleanup failed, reconciled retry, reconciled ready. Inspected cleanup is
    // tested as an additional column below. None means refusal.
    let table = [
        (
            S::Requested,
            [
                Some(S::Provisioning),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            ],
        ),
        (
            S::Provisioning,
            [
                None,
                Some(S::Ready),
                Some(S::Failed),
                None,
                None,
                None,
                Some(S::Failed),
                Some(S::Ready),
            ],
        ),
        (
            S::Ready,
            [None, None, None, Some(S::Cleaning), None, None, None, None],
        ),
        (
            S::Failed,
            [
                Some(S::Provisioning),
                None,
                None,
                Some(S::Cleaning),
                None,
                None,
                Some(S::Failed),
                Some(S::Ready),
            ],
        ),
        (
            S::Cleaning,
            [
                None,
                None,
                None,
                None,
                Some(S::Removed),
                Some(S::CleanupFailed),
                Some(S::CleanupFailed),
                None,
            ],
        ),
        (
            S::CleanupFailed,
            [
                None,
                None,
                None,
                Some(S::Cleaning),
                None,
                None,
                Some(S::CleanupFailed),
                Some(S::Ready),
            ],
        ),
        (S::Removed, [None, None, None, None, None, None, None, None]),
    ];
    for (state, outcomes) in table {
        let original = workspace_in(state);
        let operation = original.operation().cloned().unwrap_or_default();
        let observed = location(&original.intent().node);
        let events = [
            WorkspaceEvent::Provision {
                operation: operation.clone(),
                backend: observed.backend.clone(),
            },
            WorkspaceEvent::Provisioned {
                location: observed.clone(),
            },
            WorkspaceEvent::ProvisionFailed {
                retry: RetryDisposition::InspectFirst,
                location: None,
            },
            WorkspaceEvent::Cleanup {
                operation: if state == S::CleanupFailed {
                    operation
                } else {
                    OperationId::new()
                },
            },
            WorkspaceEvent::Cleaned,
            WorkspaceEvent::CleanupFailed {
                retry: RetryDisposition::InspectFirst,
            },
            WorkspaceEvent::ReconciledRetry,
            WorkspaceEvent::ReconciledReady { location: observed },
        ];
        let events = events
            .into_iter()
            .chain([WorkspaceEvent::ReconciledCleanup {
                operation: OperationId::new(),
            }]);
        let outcomes = outcomes
            .into_iter()
            .chain([(state == S::Failed).then_some(S::Cleaning)]);
        for (event, expected) in events.zip(outcomes) {
            let mut workspace = original.clone();
            let result = workspace.transition(event.clone());
            if let Some(expected) = expected {
                result.unwrap_or_else(|error| panic!("{state:?}, {event:?}: {error}"));
                assert_eq!(workspace.state(), expected);
                let json = serde_json::to_string(&workspace).unwrap();
                assert_eq!(serde_json::from_str::<Workspace>(&json).unwrap(), workspace);
            } else {
                assert!(result.is_err(), "{state:?}, {event:?}");
                assert_eq!(workspace, original);
            }
        }
    }
}

#[test]
fn uncertain_mutations_require_inspection_and_preserve_the_original_operation_key() {
    for (state, cleanup) in [
        (WorkspaceState::Failed, false),
        (WorkspaceState::CleanupFailed, true),
    ] {
        let mut workspace = workspace_in(state);
        workspace.0.retry = Some(RetryDisposition::InspectFirst);
        let operation = workspace.operation().unwrap().clone();
        let event = if cleanup {
            WorkspaceEvent::Cleanup {
                operation: operation.clone(),
            }
        } else {
            WorkspaceEvent::Provision {
                operation: operation.clone(),
                backend: workspace.backend().unwrap().clone(),
            }
        };
        assert!(workspace.transition(event.clone()).is_err());
        workspace
            .transition(WorkspaceEvent::ReconciledRetry)
            .unwrap();
        let different_key = if cleanup {
            WorkspaceEvent::Cleanup {
                operation: OperationId::new(),
            }
        } else {
            WorkspaceEvent::Provision {
                operation: OperationId::new(),
                backend: workspace.backend().unwrap().clone(),
            }
        };
        assert!(workspace.transition(different_key).is_err());
        workspace.transition(event).unwrap();
        assert_eq!(workspace.operation(), Some(&operation));
    }
    let mut manual = workspace_in(WorkspaceState::Failed);
    manual.0.retry = Some(RetryDisposition::ManualIntervention);
    assert!(manual.transition(WorkspaceEvent::ReconciledRetry).is_err());
}

#[test]
fn persisted_records_cannot_skip_ready_location_or_retry_and_node_invariants() {
    let ready = workspace_in(WorkspaceState::Ready);
    for (field, value) in [
        ("location", serde_json::Value::Null),
        ("operation", serde_json::Value::Null),
        ("backend", serde_json::Value::Null),
        ("retry", serde_json::json!("same_operation")),
    ] {
        let mut json = serde_json::to_value(&ready).unwrap();
        json[field] = value;
        assert!(serde_json::from_value::<Workspace>(json).is_err());
    }
    let mut workspace = workspace_in(WorkspaceState::Provisioning);
    let original = workspace.clone();
    assert!(
        workspace
            .transition(WorkspaceEvent::Provisioned {
                location: location(&NodeId::new())
            })
            .is_err()
    );
    assert_eq!(workspace, original);
    let mut json = serde_json::to_value(workspace_in(WorkspaceState::Failed)).unwrap();
    json["retry"] = serde_json::Value::Null;
    assert!(serde_json::from_value::<Workspace>(json).is_err());
}

#[test]
fn partial_resource_paths_and_backend_provenance_survive_failed_retries() {
    let mut workspace = workspace_in(WorkspaceState::Provisioning);
    let backend = workspace.backend().unwrap().clone();
    let operation = workspace.operation().unwrap().clone();
    let observed = location(&workspace.intent().node);
    workspace
        .transition(WorkspaceEvent::ProvisionFailed {
            retry: RetryDisposition::SameOperation,
            location: Some(observed.clone()),
        })
        .unwrap();
    assert!(
        workspace
            .transition(WorkspaceEvent::Provision {
                operation: operation.clone(),
                backend: BackendId("another-integration".to_owned()),
            })
            .is_err()
    );
    workspace
        .transition(WorkspaceEvent::Provision {
            operation,
            backend: backend.clone(),
        })
        .unwrap();
    workspace
        .transition(WorkspaceEvent::ProvisionFailed {
            retry: RetryDisposition::InspectFirst,
            location: None,
        })
        .unwrap();
    assert_eq!(workspace.location(), Some(&observed));
    assert_eq!(workspace.backend(), Some(&backend));
    let mut other_backend = observed;
    other_backend.backend = BackendId("another-integration".to_owned());
    assert!(
        workspace
            .transition(WorkspaceEvent::ReconciledReady {
                location: other_backend
            })
            .is_err()
    );
    let json = serde_json::to_string(&workspace).unwrap();
    assert_eq!(serde_json::from_str::<Workspace>(&json).unwrap(), workspace);

    let mut ready = workspace_in(WorkspaceState::Ready);
    assert!(
        ready
            .transition(WorkspaceEvent::Cleanup {
                operation: ready.operation().unwrap().clone()
            })
            .is_err()
    );
}

#[test]
fn inspected_cleanup_can_abandon_manual_only_provisioning_without_repeating_its_hooks() {
    let mut workspace = workspace_in(WorkspaceState::Provisioning);
    let operation = workspace.operation().unwrap().clone();
    let observed = location(&workspace.intent().node);
    workspace
        .transition(WorkspaceEvent::ProvisionFailed {
            retry: RetryDisposition::ManualIntervention,
            location: Some(observed.clone()),
        })
        .unwrap();
    assert!(
        workspace
            .transition(WorkspaceEvent::ReconciledRetry)
            .is_err()
    );
    assert!(
        workspace
            .transition(WorkspaceEvent::Provision {
                operation: operation.clone(),
                backend: observed.backend.clone(),
            })
            .is_err()
    );
    assert!(
        workspace
            .transition(WorkspaceEvent::Cleanup {
                operation: OperationId::new()
            })
            .is_err()
    );
    assert!(
        workspace
            .transition(WorkspaceEvent::ReconciledCleanup { operation })
            .is_err()
    );
    let cleanup = OperationId::new();
    workspace
        .transition(WorkspaceEvent::ReconciledCleanup {
            operation: cleanup.clone(),
        })
        .unwrap();
    assert_eq!(workspace.state(), WorkspaceState::Cleaning);
    assert_eq!(workspace.location(), Some(&observed));
    assert_eq!(workspace.operation(), Some(&cleanup));
    workspace.transition(WorkspaceEvent::Cleaned).unwrap();
    assert_eq!(workspace.state(), WorkspaceState::Removed);
}

#[test]
fn policies_require_explicit_backing_and_keep_stable_task_branches_across_retries() {
    let original = intent();
    let WorkspacePolicy::Worktree { repository, git } = original.policy.clone() else {
        unreachable!()
    };
    let workspace = WorkspaceId::new();
    assert_eq!(
        git.effective_branch(&workspace),
        git.effective_branch(&workspace)
    );
    assert_ne!(
        git.effective_branch(&workspace),
        git.effective_branch(&WorkspaceId::new())
    );
    let policies = [
        WorkspacePolicy::Existing {
            directory: path(&original.node, "/external"),
        },
        WorkspacePolicy::Worktree {
            repository: repository.clone(),
            git: git.clone(),
        },
        WorkspacePolicy::Clone { git: git.clone() },
        WorkspacePolicy::Ephemeral {
            backing: ManagedBacking::Worktree {
                repository,
                git: git.clone(),
            },
        },
        WorkspacePolicy::Ephemeral {
            backing: ManagedBacking::Clone { git },
        },
    ];
    for policy in policies {
        let json = serde_json::to_string(&policy).unwrap();
        assert_eq!(
            serde_json::from_str::<WorkspacePolicy>(&json).unwrap(),
            policy
        );
        let mut candidate = original.clone();
        candidate.policy = policy;
        if matches!(candidate.policy, WorkspacePolicy::Ephemeral { .. }) {
            assert!(candidate.validate().is_err());
            candidate.retention = Retention::Until { expires_at: 100 };
        }
        candidate.validate().unwrap();
    }
    for json in [
        r#"{"policy":"ephemeral"}"#,
        r#"{"policy":"existing"}"#,
        r#"{"policy":"worktree","repository":null}"#,
        r#"{"policy":"existing","directory":null,"backing":{}}"#,
    ] {
        assert!(serde_json::from_str::<WorkspacePolicy>(json).is_err());
    }
}

#[test]
fn remote_paths_use_owner_syntax_and_retention_does_not_grant_deletion_permission() {
    let node = NodeId::new();
    assert!(NodePath::new(node.clone(), PathSyntax::Windows, "C:\\repo".to_owned()).is_ok());
    assert!(
        NodePath::new(
            node.clone(),
            PathSyntax::Windows,
            "\\\\host\\share\\repo".to_owned()
        )
        .is_ok()
    );
    for value in [
        "relative",
        "C:relative",
        "\\repo",
        "\\\\host",
        "\\\\host\\",
        "C:\\bad\0path",
    ] {
        assert!(NodePath::new(node.clone(), PathSyntax::Windows, value.to_owned()).is_err());
    }
    assert!(NodePath::new(node.clone(), PathSyntax::Posix, "relative".to_owned()).is_err());
    for value in ["", "-option", "\nmain", " "] {
        assert!(GitRef::try_from(value.to_owned()).is_err());
    }
    let ready = workspace_in(WorkspaceState::Ready);
    let observed = ready.location().unwrap();
    assert_ne!(observed.root, observed.working_directory);
    assert!(!Retention::Retain.eligible_at(u64::MAX));
    assert!(
        !Retention::Idle {
            last_used_at: u64::MAX,
            idle_seconds: 1
        }
        .eligible_at(u64::MAX)
    );
    assert!(!Retention::Until { expires_at: 5 }.eligible_at(4));
    assert!(Retention::Until { expires_at: 5 }.eligible_at(5));
    assert!(
        Retention::Idle {
            last_used_at: 4,
            idle_seconds: 2
        }
        .eligible_at(6)
    );
}

fn cleanup_request(ownership: Ownership) -> CleanupRequest {
    let workspace = workspace_in(WorkspaceState::Ready);
    CleanupRequest {
        operation: OperationId::new(),
        target: ResourceTarget {
            workspace: workspace.id().clone(),
            node: workspace.intent().node.clone(),
            backend: workspace.location().unwrap().backend.clone(),
            ownership,
            location: workspace.location().cloned(),
        },
        mode: CleanupMode::PreserveChanges,
    }
}

#[test]
fn cleanup_preserves_external_dirty_unknown_and_active_resources_even_after_retention_expires() {
    let mut request = cleanup_request(Ownership::HiveManaged);
    let safe = SafetyEvidence {
        ownership_verified: true,
        cleanliness: Cleanliness::Clean,
        usage: ResourceUse::Idle,
    };
    request.check_safety(&safe).unwrap();
    let conditions = [
        (
            SafetyEvidence {
                ownership_verified: false,
                ..safe.clone()
            },
            CleanupRefusal::UnverifiedOwnership,
        ),
        (
            SafetyEvidence {
                cleanliness: Cleanliness::Dirty,
                ..safe.clone()
            },
            CleanupRefusal::Dirty,
        ),
        (
            SafetyEvidence {
                cleanliness: Cleanliness::Unknown,
                ..safe.clone()
            },
            CleanupRefusal::UnknownCleanliness,
        ),
        (
            SafetyEvidence {
                usage: ResourceUse::Active,
                ..safe.clone()
            },
            CleanupRefusal::InUse,
        ),
        (
            SafetyEvidence {
                usage: ResourceUse::Unknown,
                ..safe.clone()
            },
            CleanupRefusal::UnknownUse,
        ),
    ];
    for (evidence, expected) in conditions {
        let error = request.check_safety(&evidence).unwrap_err();
        assert!(
            matches!(error, WorkspaceError::UnsafeCleanup { reason, .. } if reason == expected)
        );
        assert_eq!(error.code(), "hive::workspace::unsafe_cleanup");
    }
    request.mode = CleanupMode::DiscardChanges;
    request
        .check_safety(&SafetyEvidence {
            cleanliness: Cleanliness::Dirty,
            ..safe.clone()
        })
        .unwrap();
    assert!(
        request
            .check_safety(&SafetyEvidence {
                cleanliness: Cleanliness::Unknown,
                ..safe.clone()
            })
            .is_err()
    );
    request.target.ownership = Ownership::External;
    assert!(matches!(
        request.check_safety(&safe),
        Err(WorkspaceError::UnsafeCleanup {
            reason: CleanupRefusal::ExternalOwnership,
            ..
        })
    ));
    request.target.ownership = Ownership::HiveManaged;
    assert!(request.target.check_node(&NodeId::new()).is_err());
    request.target.backend = BackendId("another-backend".to_owned());
    assert!(request.check_safety(&safe).is_err());
}

mod backend_contract {
    use std::{
        collections::BTreeMap,
        future::Future,
        sync::Mutex,
        task::{Context, Poll, Waker},
    };

    use super::*;

    #[derive(Default)]
    struct FakeState {
        provisions: BTreeMap<OperationId, ProvisionRequest>,
        cleanups: BTreeMap<OperationId, CleanupRequest>,
        resources: BTreeMap<WorkspaceId, FakeResource>,
        effects: usize,
        partial: bool,
        manual: bool,
    }

    struct FakeResource {
        intent: WorkspaceIntent,
        location: WorkspaceLocation,
        partial: bool,
    }

    impl FakeState {
        fn inspect_target(
            &self,
            target: &ResourceTarget,
        ) -> Result<Option<&FakeResource>, WorkspaceError> {
            target.check_node(&target.node)?;
            let resource = self.resources.get(&target.workspace);
            if let Some(resource) = resource {
                // Compare against independently stored identity, not caller-supplied evidence.
                target.check_node(resource.location.root.node())?;
                if target.backend != resource.location.backend
                    || target.ownership != resource.intent.ownership()
                    || target
                        .location
                        .as_ref()
                        .is_some_and(|hint| hint != &resource.location)
                {
                    return Err(WorkspaceError::UnsafeCleanup {
                        workspace: target.workspace.clone(),
                        reason: CleanupRefusal::IdentityConflict,
                    });
                }
            } else if target.location.as_ref().is_some_and(|hint| {
                self.resources
                    .values()
                    .any(|resource| resource.location.root == hint.root)
            }) {
                return Err(WorkspaceError::UnsafeCleanup {
                    workspace: target.workspace.clone(),
                    reason: CleanupRefusal::IdentityConflict,
                });
            }
            Ok(resource)
        }
    }

    #[derive(Default)]
    struct FakeBackend(Mutex<FakeState>);

    impl WorkspaceBackend for FakeBackend {
        fn provision(&self, request: ProvisionRequest) -> BackendFuture<'_, ProvisionResult> {
            Box::pin(async move {
                let mut state = self.0.lock().unwrap();
                request.intent.validate()?;
                if state.cleanups.contains_key(&request.operation) {
                    return Err(WorkspaceError::RetryConflict {
                        operation: request.operation,
                    });
                }
                if let Some(original) = state.provisions.get(&request.operation) {
                    request.check_retry(original)?;
                } else {
                    state
                        .provisions
                        .insert(request.operation.clone(), request.clone());
                }
                if state.manual {
                    return Err(WorkspaceError::NonIdempotent {
                        operation: request.operation,
                    });
                }
                let repeated = state.resources.contains_key(&request.workspace);
                if let Some(resource) = state.resources.get(&request.workspace) {
                    if resource.intent != request.intent
                        || resource.location.backend != request.backend
                    {
                        return Err(WorkspaceError::RetryConflict {
                            operation: request.operation,
                        });
                    }
                } else {
                    let mut observed = location(&request.intent.node);
                    // Distinct workspaces must not accidentally share the fake's checkout path.
                    let root = format!("/actual/integration/{}", request.workspace);
                    observed.root = path(&request.intent.node, &root);
                    observed.working_directory =
                        path(&request.intent.node, &format!("{root}/package"));
                    observed.backend = request.backend.clone();
                    state.resources.insert(
                        request.workspace.clone(),
                        FakeResource {
                            intent: request.intent.clone(),
                            location: observed,
                            partial: true,
                        },
                    );
                    state.effects += 1;
                }
                // Model a lost response after allocation, not a failure before any effect.
                if state.partial {
                    return Err(WorkspaceError::UncertainOperation {
                        operation: request.operation,
                    });
                }
                let resource = state.resources.get_mut(&request.workspace).unwrap();
                resource.partial = false;
                let observed = resource.location.clone();
                Ok(ProvisionResult {
                    workspace: request.workspace,
                    operation: request.operation,
                    location: observed,
                    outcome: if repeated {
                        ProvisionOutcome::AlreadySatisfied
                    } else {
                        ProvisionOutcome::Created
                    },
                })
            })
        }

        fn inspect(&self, target: ResourceTarget) -> BackendFuture<'_, Inspection> {
            Box::pin(async move {
                let state = self.0.lock().unwrap();
                let safety = SafetyEvidence {
                    ownership_verified: true,
                    cleanliness: Cleanliness::Clean,
                    usage: ResourceUse::Idle,
                };
                match state.inspect_target(&target)? {
                    Some(resource) if resource.partial => Ok(Inspection::Partial {
                        location: Some(resource.location.clone()),
                        safety,
                    }),
                    Some(resource) => Ok(Inspection::Present {
                        location: resource.location.clone(),
                        safety,
                    }),
                    None => Ok(Inspection::Absent),
                }
            })
        }

        fn cleanup(&self, request: CleanupRequest) -> BackendFuture<'_, CleanupOutcome> {
            Box::pin(async move {
                let mut state = self.0.lock().unwrap();
                if state.provisions.contains_key(&request.operation) {
                    return Err(WorkspaceError::RetryConflict {
                        operation: request.operation,
                    });
                }
                if let Some(original) = state.cleanups.get(&request.operation) {
                    request.check_retry(original)?;
                }
                state.inspect_target(&request.target)?;
                request.check_safety(&SafetyEvidence {
                    ownership_verified: true,
                    cleanliness: Cleanliness::Clean,
                    usage: ResourceUse::Idle,
                })?;
                state
                    .cleanups
                    .insert(request.operation.clone(), request.clone());
                if state.partial {
                    return Err(WorkspaceError::UncertainOperation {
                        operation: request.operation,
                    });
                }
                Ok(
                    if state.resources.remove(&request.target.workspace).is_some() {
                        CleanupOutcome::Removed
                    } else {
                        CleanupOutcome::AlreadyAbsent
                    },
                )
            })
        }
    }

    fn complete<T>(future: impl Future<Output = T>) -> T {
        let mut future = std::pin::pin!(future);
        match future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
        {
            Poll::Ready(result) => result,
            Poll::Pending => panic!("the fake backend must not depend on a runtime"),
        }
    }

    #[test]
    fn dynamic_backend_retries_reconcile_one_resource_and_reject_changed_or_cross_kind_intent() {
        let backend: Box<dyn WorkspaceBackend> = Box::<FakeBackend>::default();
        let request = ProvisionRequest {
            operation: OperationId::new(),
            workspace: WorkspaceId::new(),
            backend: BackendId("fake-integration-v1".to_owned()),
            intent: intent(),
        };
        let first = complete(backend.provision(request.clone())).unwrap();
        assert_eq!(first.outcome, ProvisionOutcome::Created);
        let retry = complete(backend.provision(request.clone())).unwrap();
        assert_eq!(retry.outcome, ProvisionOutcome::AlreadySatisfied);
        assert_eq!(first.location, retry.location);
        let mut conflict = request.clone();
        conflict.intent.retention = Retention::Until { expires_at: 3 };
        assert!(matches!(
            complete(backend.provision(conflict)),
            Err(WorkspaceError::RetryConflict { .. })
        ));
        let mut cleanup = CleanupRequest {
            operation: request.operation,
            target: ResourceTarget {
                workspace: request.workspace,
                node: request.intent.node,
                backend: first.location.backend.clone(),
                ownership: Ownership::HiveManaged,
                location: Some(first.location),
            },
            mode: CleanupMode::PreserveChanges,
        };
        assert!(matches!(
            complete(backend.cleanup(cleanup.clone())),
            Err(WorkspaceError::RetryConflict { .. })
        ));
        cleanup.operation = OperationId::new();
        assert!(matches!(
            complete(backend.inspect(cleanup.target.clone())).unwrap(),
            Inspection::Present { .. }
        ));
        assert_eq!(
            complete(backend.cleanup(cleanup.clone())).unwrap(),
            CleanupOutcome::Removed
        );
        assert_eq!(
            complete(backend.cleanup(cleanup.clone())).unwrap(),
            CleanupOutcome::AlreadyAbsent
        );
        assert_eq!(
            complete(backend.inspect(cleanup.target.clone())).unwrap(),
            Inspection::Absent
        );
        cleanup.mode = CleanupMode::DiscardChanges;
        assert!(matches!(
            complete(backend.cleanup(cleanup)),
            Err(WorkspaceError::RetryConflict { .. })
        ));
    }

    #[test]
    fn partial_effects_require_inspection_and_non_idempotent_hooks_require_manual_reconciliation() {
        let backend = FakeBackend::default();
        backend.0.lock().unwrap().partial = true;
        let request = ProvisionRequest {
            operation: OperationId::new(),
            workspace: WorkspaceId::new(),
            backend: BackendId("fake-integration-v1".to_owned()),
            intent: intent(),
        };
        let error = complete(backend.provision(request.clone())).unwrap_err();
        assert_eq!(error.code(), "hive::workspace::uncertain_operation");
        assert_eq!(error.retry(), RetryDisposition::InspectFirst);
        assert!(error.help().contains("before repeating"));
        let target = ResourceTarget {
            workspace: request.workspace.clone(),
            node: request.intent.node.clone(),
            backend: BackendId("fake-integration-v1".to_owned()),
            ownership: Ownership::HiveManaged,
            location: None,
        };
        let Inspection::Partial {
            location: Some(partial_location),
            ..
        } = complete(backend.inspect(target.clone())).unwrap()
        else {
            panic!("interrupted provisioning must leave an inspectable resource");
        };
        assert_eq!(backend.0.lock().unwrap().effects, 1);
        let cleanup = CleanupRequest {
            operation: OperationId::new(),
            target,
            mode: CleanupMode::PreserveChanges,
        };
        assert!(matches!(
            complete(backend.cleanup(cleanup.clone())),
            Err(WorkspaceError::UncertainOperation { .. })
        ));
        backend.0.lock().unwrap().partial = false;
        let recovered = complete(backend.provision(request.clone())).unwrap();
        assert_eq!(recovered.outcome, ProvisionOutcome::AlreadySatisfied);
        assert_eq!(recovered.location, partial_location);
        assert_eq!(backend.0.lock().unwrap().effects, 1);
        complete(backend.cleanup(cleanup)).unwrap();
        backend.0.lock().unwrap().manual = true;
        let error = complete(backend.provision(request)).unwrap_err();
        assert_eq!(error.code(), "hive::workspace::non_idempotent");
        assert_eq!(error.retry(), RetryDisposition::ManualIntervention);
    }

    fn target_for(request: &ProvisionRequest) -> ResourceTarget {
        ResourceTarget {
            workspace: request.workspace.clone(),
            node: request.intent.node.clone(),
            backend: request.backend.clone(),
            ownership: request.intent.ownership(),
            location: None,
        }
    }

    #[test]
    fn separate_workspaces_are_isolated_and_conflicting_targets_cannot_remove_either_resource() {
        let backend = FakeBackend::default();
        let first = ProvisionRequest {
            operation: OperationId::new(),
            workspace: WorkspaceId::new(),
            backend: BackendId("fake-integration-v1".to_owned()),
            intent: intent(),
        };
        let second = ProvisionRequest {
            operation: OperationId::new(),
            workspace: WorkspaceId::new(),
            ..first.clone()
        };
        let one = complete(backend.provision(first.clone())).unwrap();
        let two = complete(backend.provision(second.clone())).unwrap();
        assert_ne!(one.location.root, two.location.root);
        assert_eq!(backend.0.lock().unwrap().effects, 2);
        let mut conflicting = target_for(&first);
        conflicting.location = Some(two.location.clone());
        assert!(complete(backend.inspect(conflicting.clone())).is_err());
        assert!(
            complete(backend.cleanup(CleanupRequest {
                operation: OperationId::new(),
                target: conflicting,
                mode: CleanupMode::PreserveChanges,
            }))
            .is_err()
        );
        assert_eq!(backend.0.lock().unwrap().resources.len(), 2);
        let cleanup = CleanupRequest {
            operation: OperationId::new(),
            target: target_for(&first),
            mode: CleanupMode::PreserveChanges,
        };
        assert_eq!(
            complete(backend.cleanup(cleanup)).unwrap(),
            CleanupOutcome::Removed
        );
        assert_eq!(
            complete(backend.inspect(target_for(&first))).unwrap(),
            Inspection::Absent
        );
        let Inspection::Present { location, .. } =
            complete(backend.inspect(target_for(&second))).unwrap()
        else {
            panic!("cleaning the first workspace removed the second");
        };
        assert_eq!(location, two.location);
    }

    #[test]
    fn cleanup_replays_its_original_request_after_inspection_discovers_a_location() {
        let backend = FakeBackend::default();
        let provision = ProvisionRequest {
            operation: OperationId::new(),
            workspace: WorkspaceId::new(),
            backend: BackendId("fake-integration-v1".to_owned()),
            intent: intent(),
        };
        let created = complete(backend.provision(provision.clone())).unwrap();
        let original = CleanupRequest {
            operation: OperationId::new(),
            target: target_for(&provision),
            mode: CleanupMode::PreserveChanges,
        };
        backend.0.lock().unwrap().partial = true;
        assert!(matches!(
            complete(backend.cleanup(original.clone())),
            Err(WorkspaceError::UncertainOperation { .. })
        ));
        let Inspection::Present { location, .. } =
            complete(backend.inspect(original.target.clone())).unwrap()
        else {
            panic!("cleanup uncertainty must remain inspectable by identity");
        };
        assert_eq!(location, created.location);
        let mut reconstructed = original.clone();
        reconstructed.target.location = Some(location);
        assert!(matches!(
            complete(backend.cleanup(reconstructed)),
            Err(WorkspaceError::RetryConflict { .. })
        ));
        backend.0.lock().unwrap().partial = false;
        assert_eq!(
            complete(backend.cleanup(original.clone())).unwrap(),
            CleanupOutcome::Removed
        );
        assert_eq!(
            complete(backend.cleanup(original)).unwrap(),
            CleanupOutcome::AlreadyAbsent
        );
    }
}
