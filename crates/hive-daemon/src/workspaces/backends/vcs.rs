//! SCM commands stay behind the pinned repository boundary; callers supply verified checkout roots.

use super::*;
use hive_core::workspace::backend::vcs::{
    self as api, Change, DiffSection, DiffView, FileState, Identity, Inclusion, Request, State,
};
use sha2::{Digest, Sha256};
use std::io::Read;

type Result<T> = std::result::Result<T, api::Error>;
fn token(hash: Sha256) -> String {
    hash.finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
const MAX_FILE: u64 = 1024 * 1024;
const JJ_CHANGE: &str =
    "concat(json(commit_id), \"\\n\", json(change_id), \"\\n\", json(description), \"\\n\")";
const JJ_FILES: &str = "concat(\"[\", json(path), \",\", json(file_type), \",\", json(executable), \",\", json(stringify(normal_value)), \"]\\n\")";

fn git(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let mut argv = vec![
        "--no-pager",
        "--no-optional-locks",
        "--literal-pathspecs",
        "-c",
        "core.quotePath=false",
    ];
    argv.extend_from_slice(args);
    execute_output_mode("git", Some(root), &argv, false, None, false)
        .map_err(|_| api::Error::Backend)
}

fn text(bytes: Vec<u8>) -> Result<String> {
    // Invalid filenames must not be silently substituted and later target a different file.
    String::from_utf8(bytes).map_err(|_| api::Error::Unsupported)
}

fn jj(root: &Path, args: &[&str]) -> Result<String> {
    super::jj::run(root, args).map_err(|_| api::Error::Backend)
}

fn native_change(root: &Path, native_jj: bool, revision: &str) -> Result<Change> {
    if native_jj {
        let value = jj(
            root,
            &[
                "--no-integrate-operation",
                "log",
                "--no-graph",
                "-r",
                revision,
                "-T",
                JJ_CHANGE,
            ],
        )?;
        let mut lines = value.lines();
        let mut field = || {
            serde_json::from_str::<String>(lines.next().ok_or(api::Error::Backend)?)
                .map_err(|_| api::Error::Backend)
        };
        let commit_id = field()?;
        let change_id = Some(field()?);
        let message = field()?;
        let operation = jj(
            root,
            &[
                "--no-integrate-operation",
                "op",
                "log",
                "--no-graph",
                "--limit",
                "1",
                "-T",
                "id",
            ],
        )?;
        Ok(Change {
            identity: Identity {
                commit_id,
                change_id,
                operation_id: Some(operation.trim().into()),
            },
            message: bounded_message(&message).0,
            message_truncated: bounded_message(&message).1,
        })
    } else {
        let output = text(git(
            root,
            &[
                "show",
                "--no-show-signature",
                "--no-patch",
                "--format=%H%x00%B",
                revision,
                "--",
            ],
        )?)?;
        let (id, message) = output.split_once('\0').ok_or(api::Error::Backend)?;
        Ok(Change {
            identity: Identity {
                commit_id: id.into(),
                ..Identity::default()
            },
            message: bounded_message(message.trim_end_matches('\n')).0,
            message_truncated: bounded_message(message.trim_end_matches('\n')).1,
        })
    }
}

fn bounded_message(message: &str) -> (String, bool) {
    let mut end = message.len().min(16 * 1024);
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    (message[..end].into(), end < message.len())
}

fn safe_path(root: &Path, name: &str) -> Result<PathBuf> {
    let relative = Path::new(name);
    if relative.as_os_str().is_empty()
        || relative.is_absolute()
        || name.contains('\0')
        || relative
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
        || relative
            .components()
            .any(|c| c.as_os_str() == ".git" || c.as_os_str() == ".jj")
    {
        return Err(api::Error::Arguments);
    }
    let path = root.join(relative);
    // Refuse substituted directory links and nested SCMs without following them.
    for parent in path.ancestors().skip(1).take_while(|p| *p != root) {
        let metadata = match std::fs::symlink_metadata(parent) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return Err(api::Error::Backend),
        };
        if metadata.file_type().is_symlink()
            || parent.join(".git").exists()
            || parent.join(".jj").exists()
        {
            return Err(api::Error::Arguments);
        }
    }
    Ok(path)
}

fn physical(root: &Path, name: &str) -> Result<Option<(Vec<u8>, bool, bool)>> {
    let path = safe_path(root, name)?;
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(api::Error::Backend),
    };
    if metadata.len() > MAX_FILE {
        return Err(api::Error::Unsupported);
    }
    let symlink = metadata.file_type().is_symlink();
    let bytes = if symlink {
        std::fs::read_link(&path)
            .map_err(|_| api::Error::Backend)?
            .to_str()
            .ok_or(api::Error::Unsupported)?
            .as_bytes()
            .to_vec()
    } else if metadata.is_file() {
        let mut bytes = Vec::new();
        // Bound the read even if another process grows the file after metadata inspection.
        std::fs::File::open(path)
            .map_err(|_| api::Error::Backend)?
            .take(MAX_FILE + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| api::Error::Backend)?;
        if bytes.len() as u64 > MAX_FILE {
            return Err(api::Error::Unsupported);
        }
        bytes
    } else {
        return Err(api::Error::Unsupported);
    };
    #[cfg(unix)]
    let executable = {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    };
    #[cfg(not(unix))]
    let executable = false;
    Ok(Some((bytes, symlink, executable)))
}

fn file_state(path: String) -> FileState {
    FileState {
        path,
        original_path: None,
        index: None,
        working_copy: None,
        untracked: false,
        ignored: false,
        conflict: false,
    }
}

fn physical_digest(root: &Path, name: &str) -> Result<Option<(Vec<u8>, bool, bool)>> {
    let path = safe_path(root, name)?;
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(api::Error::Backend),
    };
    if metadata.file_type().is_symlink() {
        return physical(root, name).map(|value| {
            value.map(|(bytes, link, mode)| (Sha256::digest(bytes).to_vec(), link, mode))
        });
    }
    if !metadata.is_file() {
        return Err(api::Error::Unsupported);
    }
    let mut file = std::fs::File::open(path)
        .map_err(|_| api::Error::Backend)?
        .take(128 * 1024 * 1024 + 1);
    let mut hash = Sha256::new();
    let mut total = 0;
    let mut buffer = [0; 8192];
    loop {
        let count = file.read(&mut buffer).map_err(|_| api::Error::Backend)?;
        if count == 0 {
            break;
        }
        total += count;
        if total > 128 * 1024 * 1024 {
            return Err(api::Error::Unsupported);
        }
        hash.update(&buffer[..count]);
    }
    #[cfg(unix)]
    let executable = {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    };
    #[cfg(not(unix))]
    let executable = false;
    Ok(Some((hash.finalize().to_vec(), false, executable)))
}

fn git_state(root: &Path) -> Result<State> {
    let identity = native_change(root, false, "HEAD")?.identity;
    let branch = text(git(root, &["rev-parse", "--abbrev-ref", "HEAD"])?)?;
    let branch = (branch.trim() != "HEAD").then(|| branch.trim().to_owned());
    let raw = text(git(
        root,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--ignored=traditional",
            "--ignore-submodules=none",
        ],
    )?)?;
    let mut files = Vec::new();
    let mut fields = raw.split('\0').filter(|s| !s.is_empty());
    while let Some(record) = fields.next() {
        if record.len() < 4 || !record.is_char_boundary(3) {
            return Err(api::Error::Backend);
        }
        let mut file = file_state(record[3..].into());
        let status = &record.as_bytes()[..2];
        file.untracked = status == b"??";
        file.ignored = status == b"!!";
        file.conflict = status.contains(&b'U') || status == b"AA" || status == b"DD";
        if !file.untracked && !file.ignored {
            file.index = (status[0] != b' ').then(|| (status[0] as char).to_string());
            file.working_copy = (status[1] != b' ').then(|| (status[1] as char).to_string());
            if status.contains(&b'R') || status.contains(&b'C') {
                file.original_path = Some(fields.next().ok_or(api::Error::Backend)?.into());
            }
        }
        files.push(file);
    }
    let mut hash = Sha256::new();
    hash.update(identity.commit_id.as_bytes());
    hash.update(serde_json::to_vec(&branch).map_err(|_| api::Error::Backend)?);
    hash.update(raw.as_bytes());
    // Content digests detect same-status edits and partially staged index mutations.
    for args in [
        vec!["diff", "--no-ext-diff", "--no-textconv", "--binary", "--"],
        vec![
            "diff",
            "--cached",
            "--no-ext-diff",
            "--no-textconv",
            "--binary",
            "--",
        ],
    ] {
        hash.update(
            execute_output_mode("git", Some(root), &args, false, None, true)
                .map_err(|_| api::Error::Backend)?,
        );
    }
    for file in files.iter().filter(|file| file.untracked || file.ignored) {
        if !file.path.ends_with('/') {
            hash.update(
                serde_json::to_vec(&physical_digest(root, &file.path)?)
                    .map_err(|_| api::Error::Backend)?,
            );
        }
    }
    Ok(State {
        identity,
        branch,
        state_token: token(hash),
        dirty: files.iter().any(|f| !f.ignored),
        recorded_change: false,
        conflicts: files.iter().any(|f| f.conflict),
        files,
        complete: true,
    })
}

fn jj_entries(root: &Path) -> Result<Vec<(String, String, bool, String)>> {
    jj(
        root,
        &["--no-integrate-operation", "file", "list", "-T", JJ_FILES],
    )?
    .lines()
    .map(|line| serde_json::from_str(line).map_err(|_| api::Error::Backend))
    .collect()
}

fn jj_content(root: &Path, name: &str, object: Option<&str>, digest: bool) -> Result<Vec<u8>> {
    if let Some(object) = object {
        // The JJ backend explicitly uses a Git object store; this is native blob access,
        // never Git working-copy discovery or a fallback VCS selection.
        let store = jj(root, &["--no-integrate-operation", "git", "root"])?;
        return execute_output_mode(
            "git",
            Some(root),
            &["--git-dir", store.trim(), "cat-file", "blob", object],
            false,
            None,
            digest,
        )
        .map_err(|_| api::Error::Backend);
    }
    let literal = format!(
        "root:{}",
        serde_json::to_string(name).map_err(|_| api::Error::Arguments)?
    );
    super::jj::run_output(
        root,
        &[
            "--no-integrate-operation",
            "file",
            "show",
            "-r",
            "@",
            "-T",
            "\"\"",
            "--",
            &literal,
        ],
        digest,
    )
    .map_err(|_| api::Error::Backend)
}

fn walk(root: &Path, respect_ignore: bool) -> Result<BTreeSet<String>> {
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .hidden(false)
        .ignore(false)
        .parents(false)
        .require_git(false)
        .git_ignore(respect_ignore)
        .git_global(respect_ignore)
        .git_exclude(respect_ignore)
        .filter_entry(|entry| !matches!(entry.file_name().to_str(), Some(".jj" | ".git")));
    let mut paths = BTreeSet::new();
    for (count, entry) in builder.build().enumerate() {
        if count > 100_000 {
            return Err(api::Error::Unsupported);
        }
        let entry = entry.map_err(|_| api::Error::Backend)?;
        if entry.depth() > 0 && entry.file_type().is_some_and(|t| !t.is_dir()) {
            paths.insert(
                entry
                    .path()
                    .strip_prefix(root)
                    .map_err(|_| api::Error::Backend)?
                    .to_str()
                    .ok_or(api::Error::Unsupported)?
                    .replace(std::path::MAIN_SEPARATOR, "/"),
            );
        }
    }
    Ok(paths)
}

fn jj_state(root: &Path) -> Result<State> {
    let identity = native_change(root, true, "@")?.identity;
    let flags = jj(
        root,
        &[
            "--no-integrate-operation",
            "log",
            "--no-graph",
            "-r",
            "@",
            "-T",
            "concat(empty, \" \", conflict)",
        ],
    )?;
    let conflicts = flags.trim().ends_with("true");
    let mut hash = Sha256::new();
    hash.update(serde_json::to_vec(&identity).map_err(|_| api::Error::Backend)?);
    let entries = jj_entries(root)?;
    let mut known = BTreeSet::new();
    let mut files = Vec::new();
    for (name, kind, executable, object) in entries {
        known.insert(name.clone());
        let mut file = file_state(name.clone());
        if kind != "file" && kind != "symlink" {
            if conflicts {
                file.conflict = true;
                files.push(file);
                continue;
            }
            return Err(api::Error::Unsupported);
        }
        let expected = jj_content(
            root,
            &name,
            (kind == "symlink").then_some(object.as_str()),
            true,
        )?;
        let actual = physical_digest(root, &name)?;
        hash.update(name.as_bytes());
        hash.update(serde_json::to_vec(&actual).map_err(|_| api::Error::Backend)?);
        match actual {
            None => file.working_copy = Some("D".into()),
            Some((bytes, symlink, mode))
                if bytes != expected
                    || symlink != (kind == "symlink")
                    || (!symlink && mode != executable) =>
            {
                file.working_copy = Some("M".into())
            }
            _ => {}
        }
        if file.working_copy.is_some() {
            files.push(file);
        }
    }
    let visible = walk(root, true)?;
    for name in walk(root, false)?.difference(&known) {
        let mut file = file_state(name.clone());
        file.ignored = !visible.contains(name);
        file.untracked = !file.ignored;
        // Requested additions participate in preconditions even before they are tracked.
        hash.update(name.as_bytes());
        hash.update(
            serde_json::to_vec(&physical_digest(root, name)?).map_err(|_| api::Error::Backend)?,
        );
        files.push(file);
    }
    if native_change(root, true, "@")?.identity.commit_id != identity.commit_id {
        return Err(api::Error::Concurrent);
    }
    let recorded_change = !flags.trim().starts_with("true");
    Ok(State {
        identity,
        branch: None,
        state_token: token(hash),
        dirty: recorded_change || files.iter().any(|f| !f.ignored),
        recorded_change,
        conflicts,
        files,
        complete: true,
    })
}

fn state(root: &Path, native_jj: bool) -> Result<State> {
    let mut state = if native_jj {
        jj_state(root)
    } else {
        git_state(root)
    }?;
    // Status must fit within the aggregate's envelope even for enormous untracked trees.
    // A partial presentation is useful for inspection but is never commit preflight evidence.
    let mut retained = 0;
    let mut bytes = 0;
    for file in &state.files {
        bytes += serde_json::to_vec(file)
            .map_err(|_| api::Error::Backend)?
            .len();
        if retained == 1000 || bytes > 256 * 1024 {
            break;
        }
        retained += 1;
    }
    state.complete = retained == state.files.len();
    state.files.truncate(retained);
    Ok(state)
}

fn section(layer: &str, bytes: &[u8], budget: &mut usize) -> DiffSection {
    let value = String::from_utf8_lossy(bytes);
    let mut end = value.len().min(*budget);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    *budget -= end;
    DiffSection {
        layer: layer.into(),
        text: value[..end].into(),
        truncated: end < value.len(),
    }
}

fn diff_command(root: &Path, program: &str, args: &[&str]) -> Result<Vec<u8>> {
    // Only presentation output may truncate. Ownership manifests keep fail-closed capture.
    execute_output_policy(
        program,
        Some(root),
        args,
        false,
        None,
        false,
        Duration::from_secs(120),
        true,
    )
    .map_err(|_| api::Error::Backend)
}

fn diff(
    root: &Path,
    native_jj: bool,
    view: DiffView,
    mut budget: usize,
    before: &State,
) -> Result<Vec<DiffSection>> {
    if !native_jj {
        let mut args = vec![
            "--no-optional-locks",
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
        ];
        match view {
            DiffView::Staged => args.push("--cached"),
            DiffView::WorkingCopy => args.push("HEAD"),
            DiffView::Unstaged => {}
        }
        args.push("--");
        return Ok(vec![section(
            "git",
            &diff_command(root, "git", &args)?,
            &mut budget,
        )]);
    }
    if view == DiffView::Staged {
        return Err(api::Error::Unsupported);
    }
    let mut sections = Vec::new();
    if view == DiffView::WorkingCopy {
        sections.push(section(
            "recorded_change",
            &diff_command(
                root,
                "jj",
                &[
                    "--ignore-working-copy",
                    "--no-integrate-operation",
                    "--no-pager",
                    "--color=never",
                    "diff",
                    "--git",
                ],
            )?,
            &mut budget,
        ));
    }
    let entries: BTreeMap<_, _> = jj_entries(root)?
        .into_iter()
        .map(|(path, kind, mode, object)| (path, (kind, mode, object)))
        .collect();
    for file in before.files.iter().filter(|f| f.working_copy.is_some()) {
        if budget == 0 {
            if let Some(last) = sections.last_mut() {
                last.truncated = true;
            }
            break;
        }
        let entry = entries.get(&file.path).ok_or(api::Error::Concurrent)?;
        let expected = jj_content(
            root,
            &file.path,
            (entry.0 == "symlink").then_some(entry.2.as_str()),
            false,
        );
        let actual = physical(root, &file.path);
        if matches!(expected, Err(api::Error::Backend))
            || matches!(actual, Err(api::Error::Unsupported))
        {
            let mut omitted = section(
                "physical_edits",
                format!(
                    "Diff omitted for {:?}: content is unavailable or exceeds the 1 MiB display limit\n",
                    file.path
                )
                .as_bytes(),
                &mut budget,
            );
            omitted.truncated = true;
            sections.push(omitted);
            continue;
        }
        let expected = expected?;
        let physical = actual?;
        let mode_changed = physical.as_ref().is_some_and(|(_, symlink, mode)| {
            *symlink != (entry.0 == "symlink") || (!symlink && *mode != entry.1)
        });
        let actual = physical.map(|v| v.0).unwrap_or_default();
        let mut patch = format!(
            "--- recorded/{0}\n+++ physical/{0}\n",
            serde_json::to_string(&file.path).map_err(|_| api::Error::Backend)?
        );
        if mode_changed {
            patch.push_str("File type or executable mode changed\n");
        }
        if expected.contains(&0)
            || actual.contains(&0)
            || std::str::from_utf8(&expected).is_err()
            || std::str::from_utf8(&actual).is_err()
        {
            patch.push_str("Binary files differ\n");
        } else {
            let old = std::str::from_utf8(&expected).map_err(|_| api::Error::Backend)?;
            let new = std::str::from_utf8(&actual).map_err(|_| api::Error::Backend)?;
            patch.push_str(&format!(
                "@@ -1,{} +1,{} @@\n",
                old.lines().count(),
                new.lines().count()
            ));
            for line in old.lines() {
                patch.push('-');
                patch.push_str(line);
                patch.push('\n');
            }
            for line in new.lines() {
                patch.push('+');
                patch.push_str(line);
                patch.push('\n');
            }
        }
        sections.push(section("physical_edits", patch.as_bytes(), &mut budget));
        if budget == 0 {
            if let Some(last) = sections.last_mut() {
                last.truncated = true;
            }
            break;
        }
    }
    if !before.complete {
        let mut omitted = section(
            "physical_edits",
            b"Additional files omitted because status evidence is incomplete\n",
            &mut budget,
        );
        omitted.truncated = true;
        sections.push(omitted);
    }
    Ok(sections)
}

fn mutate_jj(root: &Path, args: &[&str]) -> Result<()> {
    let mut argv = vec![
        "--no-pager",
        "--color=never",
        "--config=snapshot.auto-track=none()",
        "--config=signing.behavior=drop",
    ];
    argv.extend_from_slice(args);
    execute_output_mode("jj", Some(root), &argv, false, None, false)
        .map(|_| ())
        .map_err(|_| api::Error::Uncertain)
}

fn commit(
    root: &Path,
    native_jj: bool,
    request: &Request,
    before: &State,
    authorize: &dyn Fn() -> Result<()>,
) -> Result<Change> {
    let Request::Commit {
        message,
        inclusion,
        untracked,
        ignored,
        expected_state,
    } = request
    else {
        return Err(api::Error::Arguments);
    };
    if native_jj && *inclusion != Inclusion::TrackedWorkingCopy {
        return Err(api::Error::Unsupported);
    }
    if !before.complete {
        return Err(api::Error::Unsupported);
    }
    if expected_state
        .as_ref()
        .is_some_and(|token| token != &before.state_token)
    {
        return Err(api::Error::Concurrent);
    }
    if before.conflicts {
        return Err(api::Error::Conflict);
    }
    for (paths, force) in [(untracked, false), (ignored, true)] {
        for path in paths {
            safe_path(root, path)?;
            let file = before
                .files
                .iter()
                .find(|f| &f.path == path)
                .ok_or(api::Error::Arguments)?;
            if if force {
                !file.ignored
            } else {
                !file.untracked
            } {
                return Err(api::Error::Arguments);
            }
            physical(root, path)?.ok_or(api::Error::Arguments)?;
        }
    }
    if !native_jj {
        let git_dir = text(git(root, &["rev-parse", "--absolute-git-dir"])?)?;
        if [
            "MERGE_HEAD",
            "CHERRY_PICK_HEAD",
            "REVERT_HEAD",
            "rebase-merge",
            "rebase-apply",
        ]
        .iter()
        .any(|name| Path::new(git_dir.trim()).join(name).exists())
        {
            return Err(api::Error::Conflict);
        }
        // Net selected content can be empty even with index/worktree status entries
        // (for example a staged edit reverted physically before commit --all).
        let mut argv = vec![
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--name-only",
            "-z",
        ];
        if *inclusion == Inclusion::StagedOnly {
            argv.push("--cached");
        } else {
            argv.push("HEAD");
        }
        argv.push("--");
        let selected = !git(root, &argv)?.is_empty();
        if !selected && untracked.is_empty() && ignored.is_empty() {
            return Err(api::Error::Empty);
        }
    } else if !before.recorded_change
        && !before.files.iter().any(|f| f.working_copy.is_some())
        && untracked.is_empty()
        && ignored.is_empty()
    {
        return Err(api::Error::Empty);
    }
    authorize()?;
    if state(root, native_jj)?.state_token != before.state_token {
        return Err(api::Error::Concurrent);
    }
    authorize()?;
    // Everything below may have effects even if a subsequent command fails.
    for (paths, force) in [(untracked, false), (ignored, true)] {
        for path in paths {
            if native_jj {
                let literal = format!(
                    "root:{}",
                    serde_json::to_string(path).map_err(|_| api::Error::Arguments)?
                );
                let mut argv = vec!["file", "track"];
                if force {
                    argv.push("--include-ignored");
                }
                argv.extend(["--", &literal]);
                mutate_jj(root, &argv)?;
            } else {
                let mut argv = vec!["add"];
                if force {
                    argv.push("--force");
                }
                argv.extend(["--", path]);
                git(root, &argv).map_err(|_| api::Error::Uncertain)?;
            }
        }
    }
    if native_jj {
        // Snapshot tracked physical edits without enrolling any additional paths.
        mutate_jj(root, &["status"])?;
        let flags = jj(
            root,
            &[
                "--no-integrate-operation",
                "log",
                "--no-graph",
                "-r",
                "@",
                "-T",
                "concat(empty, \" \", conflict)",
            ],
        )
        .map_err(|_| api::Error::Uncertain)?;
        if flags.trim().starts_with("true") || flags.trim().ends_with("true") {
            return Err(api::Error::Uncertain);
        }
        // Equals-bound values cannot turn a message beginning with '-' into CLI options.
        mutate_jj(root, &["commit", &format!("--message={message}")])?;
        let recorded = native_change(root, true, "@-").map_err(|_| api::Error::Uncertain)?;
        if recorded.identity.change_id != before.identity.change_id {
            return Err(api::Error::Uncertain);
        }
        Ok(recorded)
    } else {
        let message_argument = format!("--message={message}");
        let mut argv = vec!["commit", &message_argument];
        if *inclusion == Inclusion::TrackedWorkingCopy {
            argv.push("--all");
        }
        git(root, &argv).map_err(|_| api::Error::Uncertain)?;
        let recorded = native_change(root, false, "HEAD").map_err(|_| api::Error::Uncertain)?;
        let parent = text(git(root, &["rev-parse", "HEAD^"]).map_err(|_| api::Error::Uncertain)?)?;
        if parent.trim() != before.identity.commit_id {
            return Err(api::Error::Uncertain);
        }
        Ok(recorded)
    }
}

pub(super) fn execute(
    target: api::Target<'_>,
    request: &Request,
    native_jj: bool,
) -> Result<api::Result> {
    request.validate()?;
    if target.root.node() != &target.repository.node {
        return Err(api::Error::Arguments);
    }
    let root = Path::new(target.root.as_str());
    let before = state(root, native_jj)?;
    let mut result = api::Result {
        outcome: "complete".into(),
        state: before.clone(),
        recorded: None,
        diff: vec![],
        history: vec![],
    };
    match request {
        Request::Status {} => {}
        Request::Diff { view, max_bytes } => {
            result.diff = diff(root, native_jj, *view, *max_bytes, &before)?
        }
        Request::Commit { .. } => {
            result.recorded = Some(commit(
                root,
                native_jj,
                request,
                &before,
                target.authorize_mutation,
            )?);
            result.outcome = "recorded".into();
        }
        Request::Log { limit } => {
            let ids = if native_jj {
                jj(
                    root,
                    &[
                        "--no-integrate-operation",
                        "log",
                        "--no-graph",
                        "-r",
                        "ancestors(@)",
                        "--limit",
                        &limit.to_string(),
                        "-T",
                        "commit_id ++ \"\\n\"",
                    ],
                )?
            } else {
                text(git(
                    root,
                    &[
                        "log",
                        &format!("--max-count={limit}"),
                        "--format=%H",
                        "HEAD",
                        "--",
                    ],
                )?)?
            };
            for id in ids.lines() {
                result.history.push(native_change(root, native_jj, id)?);
            }
        }
    }
    match state(root, native_jj) {
        Ok(after) => {
            if !request.mutates() && after.state_token != before.state_token {
                return Err(api::Error::Concurrent);
            }
            if request.mutates() && after.branch != before.branch {
                // Keep the known commit identity while making the unexpected ref movement explicit.
                result.outcome = "recorded_concurrent".into();
            }
            result.state = after;
        }
        Err(error) if result.recorded.is_some() => {
            // Retain a known recorded ID even if the resulting physical state cannot be inspected.
            result.outcome = "recorded_state_unavailable".into();
            result.state.complete = false;
            let _ = error;
        }
        Err(error) => return Err(error),
    }
    Ok(result)
}
