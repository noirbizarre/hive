//! Git clone history preservation; the service owns the policy and durable checkpoint writer.

use super::*;

fn complete(path: &Path, plan: &ArchivePlan) -> bool {
    git(path, &["rev-parse", "HEAD"], "archive").is_ok_and(|head| head == plan.head)
        && git(
            path,
            &["for-each-ref", "--format=%(objectname) %(refname)"],
            "archive",
        )
        .is_ok_and(|refs| refs == plan.refs)
        && git(path, &["fsck", "--no-reflogs"], "archive").is_ok()
}

pub(super) fn verified(plan: &ArchivePlan, operation: &OperationId) -> bool {
    plan.destination
        .symlink_metadata()
        .is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink())
        && git(
            &plan.destination,
            &["config", "--get", "hive.archiveOperation"],
            "archive",
        )
        .is_ok_and(|v| v.trim() == operation.as_str())
        && complete(&plan.destination, plan)
}

pub(super) fn preserve(
    source: &NodePath,
    context: PreparationContext<'_>,
    checkpoints: &mut dyn CheckpointWriter<Error = Error>,
) -> Result<NodePath, Error> {
    let source_path = Path::new(source.as_str());
    let head = git(source_path, &["rev-parse", "HEAD"], "archive")?;
    let refs = git(
        source_path,
        &["for-each-ref", "--format=%(objectname) %(refname)"],
        "archive",
    )?;
    let directory = Path::new(context.directory.as_str()).join("archives");
    let destination = directory.join(format!("{}.git", context.workspace));
    let plan = if let Some(previous) = context.previous {
        let plan: ArchivePlan = previous.decode("hive.git.archive")?;
        if plan.head != head || plan.refs != refs || plan.destination != destination {
            return Err(Error::Conflict);
        }
        plan
    } else {
        let staging = directory.join(format!(
            ".{}-{}.incomplete",
            context.workspace, context.operation
        ));
        if destination.try_exists().map_err(|_| Error::Storage)?
            || staging.try_exists().map_err(|_| Error::Storage)?
        {
            return Err(Error::Conflict);
        }
        ArchivePlan {
            destination,
            staging,
            head,
            refs,
        }
    };
    // Ownership and source refs precede backup creation and rename, closing both crash windows.
    checkpoints.checkpoint(data("hive.git.archive", &plan)?)?;
    if plan.destination.exists() {
        if !verified(&plan, context.operation) {
            return Err(Error::Conflict);
        }
    } else {
        std::fs::create_dir_all(&directory).map_err(|_| Error::Removal)?;
        if plan.staging.exists() && !complete(&plan.staging, &plan) {
            // Retry only the operation-owned temporary copy while the source still matches its snapshot.
            std::fs::remove_dir_all(&plan.staging).map_err(|_| Error::Removal)?;
        }
        if !plan.staging.exists() {
            git(
                source_path,
                &[
                    "clone",
                    "--mirror",
                    "--no-hardlinks",
                    source.as_str(),
                    plan.staging.to_str().ok_or(Error::Request)?,
                ],
                "archive",
            )?;
        }
        if !complete(&plan.staging, &plan) {
            return Err(Error::Conflict);
        }
        git(
            &plan.staging,
            &[
                "config",
                "hive.archiveOperation",
                context.operation.as_str(),
            ],
            "archive",
        )?;
        #[cfg(windows)]
        git(
            &plan.staging,
            &["config", "core.longpaths", "true"],
            "archive",
        )?;
        std::fs::rename(&plan.staging, &plan.destination).map_err(|_| Error::Removal)?;
    }
    NodePath::new(
        source.node().clone(),
        source.syntax(),
        plan.destination.to_str().ok_or(Error::Request)?.into(),
    )
    .map_err(Into::into)
}
