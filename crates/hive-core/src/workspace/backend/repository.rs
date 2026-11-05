//! Repository storage and workspace mechanics have independent, persisted identities.

use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, sync::Arc};

use crate::{
    identity::{NodeId, OperationId, ProjectId, RepositoryId, WorkspaceId},
    workspace::{NodePath, WorkspaceError},
};

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct RepositoryBackendId(pub String);

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct ProvisionerId(pub String);

/// Storage compatibility is stricter than SCM compatibility (e.g. bare versus colocated).
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct StorageKind(pub String);

/// Only the selected implementation interprets the payload; orchestration preserves it verbatim.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackendData {
    pub schema: String,
    pub version: u32,
    pub data: serde_json::Value,
}

impl BackendData {
    #[must_use]
    pub fn unsupported(&self) -> WorkspaceError {
        WorkspaceError::UnsupportedBackendData {
            schema: self.schema.clone(),
            version: self.version,
        }
    }
    pub fn decode<T: serde::de::DeserializeOwned>(
        &self,
        schema: &str,
    ) -> Result<T, WorkspaceError> {
        if self.schema != schema || self.version != 1 {
            return Err(self.unsupported());
        }
        serde_json::from_value(self.data.clone()).map_err(|_| self.unsupported())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryHandle {
    pub id: RepositoryId,
    pub project: ProjectId,
    pub node: NodeId,
    pub backend: RepositoryBackendId,
    pub storage: StorageKind,
    pub metadata: BackendData,
}

impl RepositoryHandle {
    #[must_use]
    pub fn unavailable(&self) -> WorkspaceError {
        WorkspaceError::UnknownRepositoryBackend {
            backend: self.backend.0.clone(),
            storage: self.storage.0.clone(),
        }
    }
}

/// Selected before effects and never recomputed from configuration during retry or cleanup.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackendSelection {
    pub provisioner: ProvisionerId,
    pub repository: Option<RepositoryHandle>,
}

impl BackendSelection {
    #[must_use]
    pub fn incompatible(&self) -> WorkspaceError {
        WorkspaceError::IncompatibleBackends {
            provisioner: self.provisioner.0.clone(),
            backend: self.repository.as_ref().map(|r| r.backend.0.clone()),
            storage: self.repository.as_ref().map(|r| r.storage.0.clone()),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryCompatibility {
    pub backend: RepositoryBackendId,
    pub storage: StorageKind,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ProvisionerCapabilities {
    pub linked_workspace: bool,
    pub independent_checkout: bool,
    /// Lost hook responses require explicit acknowledgement, never automatic redispatch.
    pub uncertain_external_effects: bool,
    pub activity_reporting: bool,
}

#[derive(Clone, Debug)]
pub struct ProvisionerDescriptor {
    pub id: ProvisionerId,
    pub repositories: Vec<RepositoryCompatibility>,
    pub repository_optional: bool,
    pub capabilities: ProvisionerCapabilities,
}

/// Registry validation is inert: callers can reject a pair before acquiring storage.
#[derive(Default)]
pub struct BackendRegistry {
    repositories: Vec<RepositoryCompatibility>,
    provisioners: Vec<ProvisionerDescriptor>,
}

impl BackendRegistry {
    pub fn register_repository(
        &mut self,
        compatibility: RepositoryCompatibility,
    ) -> Result<(), WorkspaceError> {
        if self.repositories.contains(&compatibility) {
            return Err(WorkspaceError::DuplicateBackend);
        }
        self.repositories.push(compatibility);
        Ok(())
    }

    pub fn register_provisioner(
        &mut self,
        descriptor: ProvisionerDescriptor,
    ) -> Result<(), WorkspaceError> {
        if self.provisioners.iter().any(|p| p.id == descriptor.id) {
            return Err(WorkspaceError::DuplicateBackend);
        }
        self.provisioners.push(descriptor);
        Ok(())
    }

    pub fn resolve(
        &self,
        selection: &BackendSelection,
    ) -> Result<&ProvisionerDescriptor, WorkspaceError> {
        let descriptor = self
            .provisioners
            .iter()
            .find(|p| p.id == selection.provisioner)
            .ok_or_else(|| WorkspaceError::UnknownProvisioner {
                provisioner: selection.provisioner.0.clone(),
            })?;
        match &selection.repository {
            None if descriptor.repository_optional => Ok(descriptor),
            Some(repository) => {
                let pair = RepositoryCompatibility {
                    backend: repository.backend.clone(),
                    storage: repository.storage.clone(),
                };
                if !self.repositories.contains(&pair) {
                    return Err(repository.unavailable());
                }
                if !descriptor.repositories.contains(&pair) {
                    return Err(selection.incompatible());
                }
                Ok(descriptor)
            }
            None => Err(selection.incompatible()),
        }
    }
}

/// Synchronous implementations fit blocking execution nodes without imposing an async runtime.
/// The orchestration caller persists the handle and complete intent before invoking acquisition.
pub trait RepositoryBackend: Send + Sync {
    type Error;
    /// Inert codec validation must not acquire storage or contact a remote.
    fn validate(&self, _repository: &RepositoryHandle) -> Result<(), Self::Error> {
        Ok(())
    }
    fn verify(&self, repository: &RepositoryHandle) -> Result<(), Self::Error>;
    fn acquire(&self, repository: &RepositoryHandle) -> Result<(), Self::Error>;
    fn update(&self, repository: &RepositoryHandle) -> Result<(), Self::Error>;
    fn inspect_revision(
        &self,
        repository: &RepositoryHandle,
        intent: &BackendData,
    ) -> Result<BackendData, Self::Error>;
    fn storage_layout(
        &self,
        _repository: &RepositoryHandle,
    ) -> Result<RepositoryLayout, Self::Error> {
        Ok(RepositoryLayout::default())
    }
    fn workspace_paths(
        &self,
        _repository: &RepositoryHandle,
    ) -> Result<Vec<NodePath>, Self::Error> {
        Ok(vec![])
    }
}

/// Discovery is read-only evidence, never authority to adopt or remove unrecorded paths.
#[derive(Default)]
pub struct RepositoryLayout {
    pub scan_roots: Vec<NodePath>,
    pub staging: Vec<NodePath>,
}

/// Backend evidence excludes session usage: leases and deletion policy belong to the service.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceObservation {
    pub root: NodePath,
    pub working_directory: NodePath,
    pub metadata: BackendData,
    pub cleanliness: super::Cleanliness,
    pub ownership_verified: bool,
}

/// Durable barriers are supplied by orchestration, never by an implementation's private store.
pub trait CheckpointWriter {
    type Error;
    fn checkpoint(&mut self, data: BackendData) -> Result<(), Self::Error>;
}

pub struct PreparationContext<'a> {
    pub workspace: &'a WorkspaceId,
    pub operation: &'a OperationId,
    pub directory: &'a NodePath,
    pub previous: Option<&'a BackendData>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PreparedWorkspace {
    pub intent: BackendData,
    /// Only a planned path, never evidence of presence or authority to delete.
    pub path: Option<NodePath>,
}

pub trait WorkspaceProvisioner: Send + Sync {
    type Error;
    fn descriptor(&self) -> ProvisionerDescriptor;
    fn validate_intent(&self, _intent: &BackendData) -> Result<(), Self::Error> {
        Ok(())
    }
    fn discovery_roots(&self, _directory: &NodePath) -> Result<Vec<NodePath>, Self::Error> {
        Ok(vec![])
    }
    /// Pin backend-specific resolution before creation; checkpoints survive failed preparation.
    fn prepare(
        &self,
        _repository: Option<&RepositoryHandle>,
        intent: &BackendData,
        _context: PreparationContext<'_>,
        _checkpoints: &mut dyn CheckpointWriter<Error = Self::Error>,
    ) -> Result<PreparedWorkspace, Self::Error> {
        Ok(PreparedWorkspace {
            intent: intent.clone(),
            path: None,
        })
    }
    /// Bind inspection to an observed path without teaching orchestration backend payload schemas.
    fn at_location(
        &self,
        intent: &BackendData,
        _path: &NodePath,
        _verify_creation: bool,
    ) -> Result<BackendData, Self::Error> {
        Ok(intent.clone())
    }
    fn create(
        &self,
        repository: Option<&RepositoryHandle>,
        intent: &BackendData,
        checkpoints: &mut dyn CheckpointWriter<Error = Self::Error>,
    ) -> Result<WorkspaceObservation, Self::Error>;
    fn inspect(
        &self,
        repository: Option<&RepositoryHandle>,
        intent: &BackendData,
    ) -> Result<WorkspaceObservation, Self::Error>;
    /// Read-only proof of completed creation must not claim a partial resource or run hooks.
    fn verify_created(
        &self,
        repository: Option<&RepositoryHandle>,
        intent: &BackendData,
    ) -> Result<WorkspaceObservation, Self::Error> {
        self.inspect(repository, intent)
    }
    fn is_absent(
        &self,
        repository: Option<&RepositoryHandle>,
        intent: &BackendData,
    ) -> Result<bool, Self::Error>;
    fn prepare_cleanup(
        &self,
        _repository: Option<&RepositoryHandle>,
        _intent: &BackendData,
    ) -> Result<(), Self::Error> {
        Ok(())
    }
    /// Return an archive only when removal would otherwise lose independently stored history.
    fn preserve(
        &self,
        _repository: Option<&RepositoryHandle>,
        _intent: &BackendData,
        _context: PreparationContext<'_>,
        _checkpoints: &mut dyn CheckpointWriter<Error = Self::Error>,
    ) -> Result<Option<NodePath>, Self::Error> {
        Ok(None)
    }
    /// Linked storage normally retains history in its repository; independent storage verifies its archive.
    fn verify_preserved(
        &self,
        _archive: Option<&NodePath>,
        _checkpoint: Option<&BackendData>,
        _operation: &OperationId,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }
    /// Reconciliation supplies evidence; the service still decides lifecycle transitions.
    fn reconcile(
        &self,
        repository: Option<&RepositoryHandle>,
        intent: &BackendData,
    ) -> Result<WorkspaceObservation, Self::Error> {
        self.inspect(repository, intent)
    }
    /// Called only after fresh shared ownership, cleanliness and active-use checks.
    fn remove_checkpointed(
        &self,
        repository: Option<&RepositoryHandle>,
        intent: &BackendData,
        discard: bool,
        _operation: &OperationId,
        _checkpoints: &mut dyn CheckpointWriter<Error = Self::Error>,
    ) -> Result<(), Self::Error> {
        self.remove(repository, intent, discard)
    }
    /// Explicit cleanup retry only: finish native deregistration after proven filesystem removal.
    /// Implementations must verify the durable cleanup checkpoint before any effect.
    fn finish_removal(
        &self,
        _repository: Option<&RepositoryHandle>,
        _intent: &BackendData,
        _checkpoint: Option<&BackendData>,
        _operation: &OperationId,
    ) -> Result<bool, Self::Error> {
        Ok(false)
    }
    fn remove(
        &self,
        repository: Option<&RepositoryHandle>,
        intent: &BackendData,
        discard: bool,
    ) -> Result<(), Self::Error>;
}

/// Dispatches only recorded identities. Selection policy and durable writes stay with the caller.
pub struct BackendManager<E> {
    registry: BackendRegistry,
    repositories: BTreeMap<RepositoryBackendId, Arc<dyn RepositoryBackend<Error = E>>>,
    provisioners: BTreeMap<ProvisionerId, Arc<dyn WorkspaceProvisioner<Error = E>>>,
}

impl<E> Default for BackendManager<E> {
    fn default() -> Self {
        Self {
            registry: BackendRegistry::default(),
            repositories: BTreeMap::new(),
            provisioners: BTreeMap::new(),
        }
    }
}

impl<E: From<WorkspaceError>> BackendManager<E> {
    pub fn register_repository(
        &mut self,
        compatibility: RepositoryCompatibility,
        backend: Arc<dyn RepositoryBackend<Error = E>>,
    ) -> Result<(), E> {
        if self
            .repositories
            .get(&compatibility.backend)
            .is_some_and(|registered| !Arc::ptr_eq(registered, &backend))
        {
            return Err(WorkspaceError::DuplicateBackend.into());
        }
        self.registry.register_repository(compatibility.clone())?;
        self.repositories.insert(compatibility.backend, backend);
        Ok(())
    }

    pub fn register_provisioner(
        &mut self,
        provisioner: Arc<dyn WorkspaceProvisioner<Error = E>>,
    ) -> Result<(), E> {
        let descriptor = provisioner.descriptor();
        let id = descriptor.id.clone();
        self.registry.register_provisioner(descriptor)?;
        self.provisioners.insert(id, provisioner);
        Ok(())
    }

    pub fn resolve(&self, selection: &BackendSelection) -> Result<&ProvisionerDescriptor, E> {
        let descriptor = self.registry.resolve(selection)?;
        if let Some(repository) = &selection.repository {
            self.repository(repository)?.validate(repository)?;
        }
        Ok(descriptor)
    }

    pub fn repository(
        &self,
        repository: &RepositoryHandle,
    ) -> Result<&dyn RepositoryBackend<Error = E>, E> {
        if !self
            .registry
            .repositories
            .contains(&RepositoryCompatibility {
                backend: repository.backend.clone(),
                storage: repository.storage.clone(),
            })
        {
            return Err(repository.unavailable().into());
        }
        self.repositories
            .get(&repository.backend)
            .map(AsRef::as_ref)
            .ok_or_else(|| repository.unavailable().into())
    }

    pub fn provisioner(
        &self,
        selection: &BackendSelection,
    ) -> Result<&dyn WorkspaceProvisioner<Error = E>, E> {
        self.resolve(selection)?;
        self.provisioners
            .get(&selection.provisioner)
            .map(AsRef::as_ref)
            .ok_or_else(|| {
                WorkspaceError::UnknownProvisioner {
                    provisioner: selection.provisioner.0.clone(),
                }
                .into()
            })
    }
    pub fn provisioners(&self) -> impl Iterator<Item = &dyn WorkspaceProvisioner<Error = E>> {
        self.provisioners.values().map(AsRef::as_ref)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        identity::{OperationId, WorkspaceId},
        workspace::{
            BackendId, PathSyntax, Retention, Workspace, WorkspaceEvent, WorkspaceIntent,
            WorkspacePolicy,
        },
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn payload(schema: &str) -> BackendData {
        BackendData {
            schema: schema.into(),
            version: 1,
            data: serde_json::json!({}),
        }
    }

    struct MemoryRepository;
    impl RepositoryBackend for MemoryRepository {
        type Error = WorkspaceError;
        fn verify(&self, _: &RepositoryHandle) -> Result<(), WorkspaceError> {
            Ok(())
        }
        fn acquire(&self, _: &RepositoryHandle) -> Result<(), WorkspaceError> {
            Ok(())
        }
        fn update(&self, _: &RepositoryHandle) -> Result<(), WorkspaceError> {
            Ok(())
        }
        fn inspect_revision(
            &self,
            _: &RepositoryHandle,
            _: &BackendData,
        ) -> Result<BackendData, WorkspaceError> {
            Ok(payload("memory.change"))
        }
    }

    struct MemoryWorkspace {
        effects: Arc<AtomicUsize>,
        path: NodePath,
    }
    impl WorkspaceProvisioner for MemoryWorkspace {
        type Error = WorkspaceError;
        fn descriptor(&self) -> ProvisionerDescriptor {
            ProvisionerDescriptor {
                id: ProvisionerId("memory.workspace".into()),
                repositories: vec![RepositoryCompatibility {
                    backend: RepositoryBackendId("memory".into()),
                    storage: StorageKind("memory.store".into()),
                }],
                repository_optional: false,
                capabilities: ProvisionerCapabilities::default(),
            }
        }
        fn create(
            &self,
            repository: Option<&RepositoryHandle>,
            intent: &BackendData,
            checkpoints: &mut dyn CheckpointWriter<Error = WorkspaceError>,
        ) -> Result<WorkspaceObservation, WorkspaceError> {
            checkpoints.checkpoint(payload("memory.prepared"))?;
            self.effects.fetch_add(1, Ordering::SeqCst);
            self.inspect(repository, intent)
        }
        fn inspect(
            &self,
            _: Option<&RepositoryHandle>,
            _: &BackendData,
        ) -> Result<WorkspaceObservation, WorkspaceError> {
            Ok(WorkspaceObservation {
                root: self.path.clone(),
                working_directory: self.path.clone(),
                metadata: payload("memory.observation"),
                cleanliness: super::super::Cleanliness::Clean,
                ownership_verified: true,
            })
        }
        fn remove(
            &self,
            _: Option<&RepositoryHandle>,
            _: &BackendData,
            _: bool,
        ) -> Result<(), WorkspaceError> {
            self.effects.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn is_absent(
            &self,
            _: Option<&RepositoryHandle>,
            _: &BackendData,
        ) -> Result<bool, WorkspaceError> {
            Ok(self.effects.load(Ordering::SeqCst) == 2)
        }
    }

    struct Checkpoints {
        fail: bool,
    }
    impl CheckpointWriter for Checkpoints {
        type Error = WorkspaceError;
        fn checkpoint(&mut self, _: BackendData) -> Result<(), WorkspaceError> {
            if self.fail {
                Err(WorkspaceError::InvalidRecord)
            } else {
                Ok(())
            }
        }
    }

    fn fixture() -> (
        BackendManager<WorkspaceError>,
        BackendSelection,
        Arc<AtomicUsize>,
        NodePath,
    ) {
        let repository = RepositoryHandle {
            id: RepositoryId::new(),
            project: ProjectId::new(),
            node: NodeId::new(),
            backend: RepositoryBackendId("memory".into()),
            storage: StorageKind("memory.store".into()),
            metadata: payload("memory.repository"),
        };
        let path = NodePath::new(
            repository.node.clone(),
            PathSyntax::Posix,
            "/memory/workspace".into(),
        )
        .unwrap();
        let effects = Arc::new(AtomicUsize::new(0));
        let mut manager = BackendManager::default();
        manager
            .register_repository(
                RepositoryCompatibility {
                    backend: repository.backend.clone(),
                    storage: repository.storage.clone(),
                },
                Arc::new(MemoryRepository),
            )
            .unwrap();
        manager
            .register_provisioner(Arc::new(MemoryWorkspace {
                effects: effects.clone(),
                path: path.clone(),
            }))
            .unwrap();
        (
            manager,
            BackendSelection {
                repository: Some(repository),
                provisioner: ProvisionerId("memory.workspace".into()),
            },
            effects,
            path,
        )
    }

    #[test]
    fn a_non_git_backend_uses_the_shared_lifecycle_without_git_intent_or_observations() {
        let (manager, selection, effects, path) = fixture();
        let repository = selection.repository.as_ref().unwrap();
        let intent = payload("memory.change");
        let mut workspace = Workspace::new(
            WorkspaceId::new(),
            WorkspaceIntent {
                project: repository.project.clone(),
                node: repository.node.clone(),
                policy: WorkspacePolicy::Managed {
                    selection: Box::new(selection.clone()),
                    intent: intent.clone(),
                },
                retention: Retention::Retain,
            },
        )
        .unwrap();
        workspace
            .transition(WorkspaceEvent::Provision {
                operation: OperationId::new(),
                backend: BackendId(selection.provisioner.0.clone()),
            })
            .unwrap();
        manager
            .repository(repository)
            .unwrap()
            .acquire(repository)
            .unwrap();
        let provisioner = manager.provisioner(&selection).unwrap();
        let observed = provisioner
            .create(Some(repository), &intent, &mut Checkpoints { fail: false })
            .unwrap();
        assert_eq!(observed.root, path);
        assert_eq!(
            provisioner.reconcile(Some(repository), &intent).unwrap(),
            observed
        );
        let location = crate::workspace::WorkspaceLocation {
            root: observed.root.clone(),
            working_directory: observed.working_directory.clone(),
            backend: workspace.backend().unwrap().clone(),
            observations: Some(observed.metadata.clone()),
            resolved_base: None,
            branch: None,
            commit: None,
        };
        workspace
            .transition(WorkspaceEvent::Provisioned {
                location: location.clone(),
            })
            .unwrap();
        let serialized = serde_json::to_value(&workspace).unwrap();
        assert!(serialized["intent"]["policy"].get("git").is_none());
        assert_eq!(
            serde_json::from_value::<Workspace>(serialized).unwrap(),
            workspace
        );
        let cleanup = super::super::CleanupRequest {
            operation: OperationId::new(),
            target: super::super::ResourceTarget {
                workspace: workspace.id().clone(),
                node: repository.node.clone(),
                backend: workspace.backend().unwrap().clone(),
                ownership: crate::workspace::Ownership::HiveManaged,
                location: Some(location),
            },
            mode: super::super::CleanupMode::PreserveChanges,
        };
        cleanup
            .check_safety(&super::super::SafetyEvidence {
                ownership_verified: observed.ownership_verified,
                cleanliness: observed.cleanliness,
                usage: super::super::ResourceUse::Idle,
            })
            .unwrap();
        workspace
            .transition(WorkspaceEvent::Cleanup {
                operation: cleanup.operation,
            })
            .unwrap();
        provisioner
            .remove(Some(repository), &intent, false)
            .unwrap();
        assert!(provisioner.is_absent(Some(repository), &intent).unwrap());
        workspace.transition(WorkspaceEvent::Cleaned).unwrap();
        assert_eq!(workspace.state(), crate::workspace::WorkspaceState::Removed);
        assert_eq!(effects.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn incompatible_or_missing_pinned_implementations_fail_before_effects() {
        let (mut manager, selection, effects, _) = fixture();
        let mut incompatible = selection.clone();
        let repository = incompatible.repository.as_mut().unwrap();
        repository.backend = RepositoryBackendId("other".into());
        manager
            .register_repository(
                RepositoryCompatibility {
                    backend: repository.backend.clone(),
                    storage: repository.storage.clone(),
                },
                Arc::new(MemoryRepository),
            )
            .unwrap();
        assert!(matches!(
            manager.provisioner(&incompatible),
            Err(WorkspaceError::IncompatibleBackends { .. })
        ));
        incompatible.provisioner = ProvisionerId("missing".into());
        assert!(matches!(
            manager.provisioner(&incompatible),
            Err(WorkspaceError::UnknownProvisioner { .. })
        ));
        let mut missing = selection.clone();
        missing.repository = None;
        assert!(matches!(
            manager.resolve(&missing),
            Err(WorkspaceError::IncompatibleBackends { .. })
        ));
        assert_eq!(effects.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn failed_durable_barriers_prevent_backend_effects_and_payload_versions_are_not_guessed() {
        let (manager, selection, effects, _) = fixture();
        let provisioner = manager.provisioner(&selection).unwrap();
        assert!(
            provisioner
                .create(
                    selection.repository.as_ref(),
                    &payload("memory.change"),
                    &mut Checkpoints { fail: true }
                )
                .is_err()
        );
        assert_eq!(effects.load(Ordering::SeqCst), 0);
        let mut future = payload("memory.change");
        future.version = 2;
        future.data = serde_json::json!({"private":"never-echo-payload"});
        assert_eq!(
            future.decode::<serde_json::Value>("memory.change"),
            Err(future.unsupported())
        );
        let message = future.unsupported().to_string();
        assert!(message.contains("memory.change") && message.contains('2'));
        assert!(!message.contains("never-echo-payload"));
    }

    #[test]
    fn duplicate_registration_cannot_replace_a_pinned_implementation() {
        let (mut manager, selection, effects, path) = fixture();
        assert_eq!(
            manager.register_provisioner(Arc::new(MemoryWorkspace {
                effects: Arc::new(AtomicUsize::new(0)),
                path
            })),
            Err(WorkspaceError::DuplicateBackend)
        );
        manager
            .provisioner(&selection)
            .unwrap()
            .create(
                selection.repository.as_ref(),
                &payload("memory.change"),
                &mut Checkpoints { fail: false },
            )
            .unwrap();
        assert_eq!(effects.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn managed_records_reject_conflicting_provisioners_without_mutating_their_lifecycle() {
        let (_, selection, _, _) = fixture();
        let repository = selection.repository.as_ref().unwrap();
        for policy in [
            WorkspacePolicy::Managed {
                selection: Box::new(selection.clone()),
                intent: payload("memory.change"),
            },
            WorkspacePolicy::Ephemeral {
                backing: crate::workspace::ManagedBacking::Backend {
                    selection: Box::new(selection.clone()),
                    intent: payload("memory.change"),
                },
            },
        ] {
            let mut workspace = Workspace::new(
                WorkspaceId::new(),
                WorkspaceIntent {
                    project: repository.project.clone(),
                    node: repository.node.clone(),
                    policy,
                    retention: Retention::Until { expires_at: 1 },
                },
            )
            .unwrap();
            let original = workspace.clone();
            let operation = OperationId::new();
            assert_eq!(
                workspace.transition(WorkspaceEvent::Provision {
                    operation: operation.clone(),
                    backend: BackendId("other.workspace".into())
                }),
                Err(WorkspaceError::InvalidRecord)
            );
            assert_eq!(workspace, original);
            workspace
                .transition(WorkspaceEvent::Provision {
                    operation,
                    backend: BackendId(selection.provisioner.0.clone()),
                })
                .unwrap();
            let mut record = serde_json::to_value(&workspace).unwrap();
            record["backend"] = "other.workspace".into();
            assert!(serde_json::from_value::<Workspace>(record).is_err());
        }
        assert_eq!(
            BackendId("native".into()).provisioner(),
            ProvisionerId("git.worktree".into())
        );
        assert_eq!(
            BackendId("clone".into()).provisioner(),
            ProvisionerId("git.clone".into())
        );
    }
}
