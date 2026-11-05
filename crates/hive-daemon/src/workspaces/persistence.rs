//! Upgrade provenance without rewriting immutable operation or sweep request snapshots.

use super::*;

impl Runtime<'_> {
    pub(super) fn repository_handle<'a>(
        &self,
        repository: &'a Repository,
    ) -> Result<&'a RepositoryHandle, Error> {
        let handle = repository.handle.as_ref().ok_or(Error::Conflict)?;
        if handle.node != self.state.node
            || repository.repository.node() != &self.state.node
            || handle.id != repository.repository.id
            || handle.project != repository.repository.project
        {
            return Err(Error::Conflict);
        }
        Ok(handle)
    }
    pub(super) fn repository_layout(
        &self,
        repository: &Repository,
    ) -> Result<hive_core::workspace::backend::RepositoryLayout, Error> {
        let handle = self.repository_handle(repository)?;
        let layout = self.manager.repository(handle)?.storage_layout(handle)?;
        for path in layout.scan_roots.iter().chain(&layout.staging) {
            if path.node() != &self.state.node {
                return Err(Error::Conflict);
            }
        }
        Ok(layout)
    }
    pub(super) fn cleanup_checkpoint(
        &self,
        operation: &OperationId,
    ) -> Result<Option<BackendData>, Error> {
        if let Some(prepared) = self
            .state
            .operations
            .get(operation.as_str())
            .and_then(|o| o.prepared.clone())
        {
            return Ok(Some(prepared));
        }
        // Legacy archive plans lived outside operations; keep this compatibility knowledge at storage's edge.
        self.state
            .archives
            .get(operation.as_str())
            .map(|plan| backends::data("hive.git.archive", plan))
            .transpose()
    }
    pub(super) fn uncertain_backend(&self, entry: &Entry) -> Result<bool, Error> {
        let selection = self.selection(entry)?;
        Ok(self
            .manager
            .resolve(&selection)?
            .capabilities
            .uncertain_external_effects)
    }
    pub(super) fn migrate_backends(&mut self) -> Result<bool, Error> {
        if self.state.version > 2 {
            return Err(
                hive_core::workspace::WorkspaceError::UnsupportedBackendData {
                    schema: "hive.workspace.state".into(),
                    version: self.state.version,
                }
                .into(),
            );
        }
        // A malformed reservation must never disappear from deletion eligibility checks.
        for (id, reservation) in &self.state.mcp_reservations {
            if uuid::Uuid::parse_str(id).is_err()
                || reservation.session.is_empty()
                || reservation.workspaces.is_empty()
                || reservation
                    .workspaces
                    .iter()
                    .any(|workspace| !self.state.workspaces.contains_key(workspace.as_str()))
            {
                return Err(Error::Conflict);
            }
        }
        if self.state.version == 2 {
            return Ok(false);
        }
        if self.state.version == 1 {
            // Older daemons must refuse snapshots whose independent MCP holds they cannot enforce.
            self.state.version = 2;
            return Ok(true);
        }
        for repository in self.state.repositories.values_mut() {
            repository.handle = Some(backends::handle(repository)?);
        }
        for entry in self.state.workspaces.values_mut() {
            let backend = entry.workspace.backend().ok_or(Error::Conflict)?;
            let provisioner = legacy_provisioner(backend)?;
            let repository = entry
                .repository
                .as_ref()
                .map(|id| {
                    self.state
                        .repositories
                        .get(id.as_str())
                        .and_then(|r| r.handle.clone())
                        .ok_or(Error::Conflict)
                })
                .transpose()?;
            let selection = BackendSelection {
                provisioner,
                repository,
            };
            self.manager.resolve(&selection)?;
            entry.selection = Some(selection);
        }
        for operation in self.state.operations.values_mut() {
            // Only the legacy decoder inspects request shape; new recovery uses explicit kinds.
            operation.kind = Some(if operation.request.get("project").is_some() {
                OperationKind::Create
            } else {
                OperationKind::Cleanup
            });
            operation.selection = Some(
                self.state
                    .workspaces
                    .get(operation.workspace.as_str())
                    .and_then(|entry| entry.selection.clone())
                    .ok_or(Error::Conflict)?,
            );
            operation.prepared = backends::legacy_checkpoint(
                self.state
                    .workspaces
                    .get(operation.workspace.as_str())
                    .ok_or(Error::Conflict)?,
                operation,
                self.store.state_directory(),
            )?;
        }
        // The admission caller publishes this entire snapshot before permitting any side effects.
        self.state.version = 2;
        Ok(true)
    }

    pub(super) fn selection(&self, entry: &Entry) -> Result<BackendSelection, Error> {
        let selection = entry.selection.clone().ok_or(Error::Conflict)?;
        self.manager.resolve(&selection)?;
        let backend = entry.workspace.backend().ok_or(Error::Conflict)?;
        let expected = if matches!(
            backend.0.as_str(),
            "native" | "clone" | "worktrunk" | "existing"
        ) {
            legacy_provisioner(backend)?
        } else {
            ProvisionerId(backend.0.clone())
        };
        if expected != selection.provisioner {
            return Err(Error::Conflict);
        }
        if entry
            .workspace
            .intent()
            .selection()
            .is_some_and(|pinned| pinned != &selection)
        {
            return Err(Error::Conflict);
        }
        if let Some(repository) = &selection.repository {
            if repository.node != self.state.node
                || repository.project != entry.workspace.intent().project
                || entry.repository.as_ref() != Some(&repository.id)
                || self
                    .state
                    .repositories
                    .get(repository.id.as_str())
                    .and_then(|r| r.handle.as_ref())
                    != Some(repository)
            {
                return Err(Error::Conflict);
            }
        } else if entry.repository.is_some()
            || entry.workspace.intent().ownership() != Ownership::External
        {
            return Err(Error::Conflict);
        }
        if entry.workspace.intent().node != self.state.node {
            return Err(Error::Conflict);
        }
        if self.state.operations.values().any(|operation| {
            &operation.workspace == entry.workspace.id()
                && operation.selection.as_ref() != Some(&selection)
        }) {
            return Err(Error::Conflict);
        }
        Ok(selection)
    }
}

pub(super) fn legacy_provisioner(backend: &BackendId) -> Result<ProvisionerId, Error> {
    Ok(ProvisionerId(
        match backend.0.as_str() {
            "native" => "git.worktree",
            "clone" => "git.clone",
            "worktrunk" => "worktrunk",
            "existing" => "external.directory",
            _ => {
                return Err(hive_core::workspace::WorkspaceError::UnknownProvisioner {
                    provisioner: backend.0.clone(),
                }
                .into());
            }
        }
        .into(),
    ))
}

/// Implementations can publish observations but cannot alter requests, leases or lifecycle state.
pub(super) struct OperationCheckpoints<'a, 'b> {
    pub runtime: &'a mut Runtime<'b>,
    pub key: &'a str,
}

impl CheckpointWriter for OperationCheckpoints<'_, '_> {
    type Error = Error;
    fn checkpoint(&mut self, data: BackendData) -> Result<(), Error> {
        let archive = if data.schema == "hive.git.archive" {
            Some(data.decode::<ArchivePlan>("hive.git.archive")?)
        } else {
            None
        };
        let operation = self
            .runtime
            .state
            .operations
            .get_mut(self.key)
            .ok_or(Error::Conflict)?;
        match data.schema.as_str() {
            "hive.workspace.effects" => {
                if !data.decode::<bool>("hive.workspace.effects")? {
                    return Err(Error::Conflict);
                }
                operation.effects_started = true;
            }
            "hive.workspace.path" => {
                let path: PathBuf = data.decode("hive.workspace.path")?;
                if !path.is_absolute() {
                    return Err(Error::Conflict);
                }
                operation.path = Some(path);
            }
            _ => {
                // Legacy commit is a response/recovery projection; the authoritative checkpoint is opaque.
                if data.schema == "hive.git.commit" {
                    operation.commit = Some(data.decode("hive.git.commit")?);
                }
                operation.prepared = Some(data);
            }
        }
        if let Some(plan) = archive {
            self.runtime.state.archives.insert(self.key.into(), plan);
        }
        self.runtime.save()
    }
}
