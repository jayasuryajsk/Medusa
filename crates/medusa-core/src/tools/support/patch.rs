use super::*;

/// Compact single-line JSON preview of MCP call arguments for approval cards.
pub(crate) fn mcp_arguments_preview(arguments: &serde_json::Value) -> String {
    let rendered = serde_json::to_string(arguments).unwrap_or_default();
    compact_text(&rendered, 120)
}

pub(crate) fn compact_text(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let compacted = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{compacted}...")
    } else {
        compacted
    }
}

pub(crate) fn run_git_apply(cwd: &Path, diff: &str, check: bool, recount: bool) -> Result<()> {
    let mut command = Command::new("git");
    command.arg("apply");
    if check {
        command.arg("--check");
    }
    if recount {
        command.arg("--recount");
    }

    let mut child = command
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .wrap_err("failed to start git apply")?;

    child
        .stdin
        .as_mut()
        .expect("stdin was configured")
        .write_all(diff.as_bytes())
        .wrap_err("failed to send patch to git apply")?;

    let output = child.wait_with_output().wrap_err("git apply failed")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git apply rejected patch: {}", stderr.trim());
    }

    Ok(())
}

pub(crate) fn normalize_patch(diff: &str) -> String {
    let mut diff = diff.trim().to_string();

    if diff.starts_with("```") {
        let mut lines = diff.lines().collect::<Vec<_>>();
        if lines
            .first()
            .is_some_and(|line| line.trim_start().starts_with("```"))
        {
            lines.remove(0);
        }
        if lines
            .last()
            .is_some_and(|line| line.trim_start().starts_with("```"))
        {
            lines.pop();
        }
        diff = lines.join("\n");
    }

    if let Some(start) = diff.find("diff --git ") {
        diff = diff[start..].to_string();
    }

    if !diff.ends_with('\n') {
        diff.push('\n');
    }

    diff
}

pub(crate) fn is_codex_patch(diff: &str) -> bool {
    diff.trim_start().starts_with("*** Begin Patch")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PatchOp {
    Add {
        path: String,
        content: String,
    },
    Delete {
        path: String,
    },
    Update {
        path: String,
        move_to: Option<String>,
        hunks: Vec<PatchHunk>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PatchHunk {
    old: String,
    new: String,
}

pub(crate) fn apply_codex_patch(workspace: &Path, diff: &str) -> Result<Vec<String>> {
    let ops = parse_codex_patch(diff)?;
    let snapshots = snapshot_codex_patch_paths(workspace, &ops)?;
    match apply_codex_patch_ops(workspace, ops) {
        Ok(changed) => Ok(changed),
        Err(apply_error) => match restore_codex_patch_paths(&snapshots) {
            Ok(()) => Err(apply_error.wrap_err("Codex patch rolled back without changes")),
            Err(rollback_error) => Err(apply_error.wrap_err(format!(
                "Codex patch failed and rollback was incomplete: {rollback_error:#}"
            ))),
        },
    }
}

#[derive(Debug)]
pub(crate) struct PatchPathSnapshot {
    path: PathBuf,
    content: Option<Vec<u8>>,
    permissions: Option<fs::Permissions>,
}

pub(crate) fn snapshot_codex_patch_paths(
    workspace: &Path,
    ops: &[PatchOp],
) -> Result<Vec<PatchPathSnapshot>> {
    let mut paths = BTreeSet::new();
    for op in ops {
        match op {
            PatchOp::Add { path, .. } | PatchOp::Delete { path } => {
                validate_relative_path(path)?;
                paths.insert(path.as_str());
            }
            PatchOp::Update { path, move_to, .. } => {
                validate_relative_path(path)?;
                paths.insert(path.as_str());
                if let Some(move_to) = move_to {
                    validate_relative_path(move_to)?;
                    paths.insert(move_to.as_str());
                }
            }
        }
    }

    paths
        .into_iter()
        .map(|relative| {
            let path = workspace.join(relative);
            match fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    bail!("patch path is a symlink; refusing to follow it: {relative}")
                }
                Ok(metadata) if !metadata.is_file() => {
                    bail!("patch target is not a regular file: {relative}")
                }
                Ok(metadata) => Ok(PatchPathSnapshot {
                    content: Some(
                        fs::read(&path)
                            .wrap_err_with(|| format!("failed to snapshot {}", path.display()))?,
                    ),
                    permissions: Some(metadata.permissions()),
                    path,
                }),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    Ok(PatchPathSnapshot {
                        path,
                        content: None,
                        permissions: None,
                    })
                }
                Err(error) => Err(error)
                    .wrap_err_with(|| format!("failed to inspect patch path {}", path.display())),
            }
        })
        .collect()
}

pub(crate) fn restore_codex_patch_paths(snapshots: &[PatchPathSnapshot]) -> Result<()> {
    let mut failures = Vec::new();
    for snapshot in snapshots.iter().rev() {
        let result = match &snapshot.content {
            Some(content) => atomic_write(&snapshot.path, content).and_then(|()| {
                if let Some(permissions) = &snapshot.permissions {
                    fs::set_permissions(&snapshot.path, permissions.clone()).wrap_err_with(
                        || {
                            format!(
                                "failed to restore permissions for {}",
                                snapshot.path.display()
                            )
                        },
                    )?;
                }
                Ok(())
            }),
            None => match fs::symlink_metadata(&snapshot.path) {
                Ok(metadata) if metadata.is_file() || metadata.file_type().is_symlink() => {
                    fs::remove_file(&snapshot.path)
                        .wrap_err_with(|| format!("failed to remove {}", snapshot.path.display()))
                }
                Ok(_) => Err(color_eyre::eyre::eyre!(
                    "rollback target became a directory: {}",
                    snapshot.path.display()
                )),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error)
                    .wrap_err_with(|| format!("failed to inspect {}", snapshot.path.display())),
            },
        };
        if let Err(error) = result {
            failures.push(error.to_string());
        }
    }

    if failures.is_empty() {
        Ok(())
    } else {
        bail!("{}", failures.join("; "))
    }
}

pub(crate) fn apply_codex_patch_ops(workspace: &Path, ops: Vec<PatchOp>) -> Result<Vec<String>> {
    let mut changed = BTreeSet::new();

    for op in ops {
        match op {
            PatchOp::Add { path, content } => {
                validate_relative_path(&path)?;
                let resolved = workspace.join(&path);
                if resolved.exists() {
                    bail!("Add File target already exists: {path}");
                }
                if let Some(parent) = resolved.parent() {
                    fs::create_dir_all(parent)
                        .wrap_err_with(|| format!("failed to create {}", parent.display()))?;
                }
                atomic_write(&resolved, content)
                    .wrap_err_with(|| format!("failed to write {}", resolved.display()))?;
                changed.insert(path);
            }
            PatchOp::Delete { path } => {
                validate_relative_path(&path)?;
                let resolved = workspace.join(&path);
                if !resolved.is_file() {
                    bail!("Delete File target is not a file: {path}");
                }
                fs::remove_file(&resolved)
                    .wrap_err_with(|| format!("failed to delete {}", resolved.display()))?;
                changed.insert(path);
            }
            PatchOp::Update {
                path,
                move_to,
                hunks,
            } => {
                validate_relative_path(&path)?;
                if let Some(ref target) = move_to {
                    validate_relative_path(target)?;
                }
                let resolved = workspace.join(&path);
                if !resolved.is_file() {
                    bail!("Update File target is not a file: {path}");
                }
                let mut content = fs::read_to_string(&resolved)
                    .wrap_err_with(|| format!("failed to read {}", resolved.display()))?;
                for hunk in hunks {
                    if hunk.old.is_empty() {
                        bail!("Update File hunk for {path} has no removable/context lines");
                    }
                    let Some(index) = content.find(&hunk.old) else {
                        bail!(
                            "Update File hunk did not match current content in {path}; re-read the file and retry with exact context"
                        );
                    };
                    content.replace_range(index..index + hunk.old.len(), &hunk.new);
                }

                let final_path = move_to.unwrap_or_else(|| path.clone());
                let final_resolved = workspace.join(&final_path);
                if final_path != path {
                    if final_resolved.exists() {
                        bail!("Move target already exists: {final_path}");
                    }
                    if let Some(parent) = final_resolved.parent() {
                        fs::create_dir_all(parent)
                            .wrap_err_with(|| format!("failed to create {}", parent.display()))?;
                    }
                }
                atomic_write(&final_resolved, content)
                    .wrap_err_with(|| format!("failed to write {}", final_resolved.display()))?;
                if final_path != path {
                    fs::remove_file(&resolved).wrap_err_with(|| {
                        format!("failed to remove moved source {}", resolved.display())
                    })?;
                }
                changed.insert(path);
                changed.insert(final_path);
            }
        }
    }

    Ok(changed.into_iter().collect())
}

pub(crate) fn parse_codex_patch(diff: &str) -> Result<Vec<PatchOp>> {
    let lines = diff.lines().collect::<Vec<_>>();
    if lines.first().map(|line| line.trim()) != Some("*** Begin Patch") {
        bail!("Codex patch must start with *** Begin Patch");
    }
    let mut index = 1;
    let mut ops = Vec::new();
    while index < lines.len() {
        let line = lines[index].trim_end();
        if line == "*** End Patch" {
            return Ok(ops);
        }
        if line == "*** End of File" {
            index += 1;
            continue;
        }
        if let Some(path) = line.strip_prefix("*** Add File: ") {
            index += 1;
            let mut content = String::new();
            while index < lines.len() && !lines[index].starts_with("*** ") {
                let line = lines[index];
                content.push_str(line.strip_prefix('+').unwrap_or(line));
                content.push('\n');
                index += 1;
            }
            ops.push(PatchOp::Add {
                path: path.trim().to_string(),
                content,
            });
            continue;
        }
        if let Some(path) = line.strip_prefix("*** Delete File: ") {
            ops.push(PatchOp::Delete {
                path: path.trim().to_string(),
            });
            index += 1;
            continue;
        }
        if let Some(path) = line.strip_prefix("*** Update File: ") {
            index += 1;
            let mut move_to = None;
            let mut hunks = Vec::new();
            if index < lines.len()
                && let Some(target) = lines[index].trim_end().strip_prefix("*** Move to: ")
            {
                move_to = Some(target.trim().to_string());
                index += 1;
            }
            while index < lines.len() && !lines[index].starts_with("*** ") {
                if lines[index].starts_with("@@") {
                    index += 1;
                }
                let mut old = String::new();
                let mut new = String::new();
                while index < lines.len()
                    && !lines[index].starts_with("@@")
                    && !lines[index].starts_with("*** ")
                {
                    let line = lines[index];
                    if let Some(rest) = line.strip_prefix('-') {
                        old.push_str(rest);
                        old.push('\n');
                    } else if let Some(rest) = line.strip_prefix('+') {
                        new.push_str(rest);
                        new.push('\n');
                    } else {
                        let rest = line.strip_prefix(' ').unwrap_or(line);
                        old.push_str(rest);
                        old.push('\n');
                        new.push_str(rest);
                        new.push('\n');
                    }
                    index += 1;
                }
                if !old.is_empty() || !new.is_empty() {
                    hunks.push(PatchHunk { old, new });
                }
            }
            if hunks.is_empty() && move_to.is_none() {
                bail!("Update File requires at least one hunk or Move to: {path}");
            }
            ops.push(PatchOp::Update {
                path: path.trim().to_string(),
                move_to,
                hunks,
            });
            continue;
        }
        bail!("unrecognized Codex patch line: {line}");
    }
    bail!("Codex patch missing *** End Patch")
}

pub(crate) fn extract_patch_paths(diff: &str) -> Result<Vec<String>> {
    let mut paths = BTreeSet::new();

    if is_codex_patch(diff) {
        for op in parse_codex_patch(diff)? {
            match op {
                PatchOp::Add { path, .. } | PatchOp::Delete { path } => {
                    paths.insert(path);
                }
                PatchOp::Update { path, move_to, .. } => {
                    paths.insert(path);
                    if let Some(move_to) = move_to {
                        paths.insert(move_to);
                    }
                }
            }
        }
        return Ok(paths.into_iter().collect());
    }

    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            // Capture BOTH sides. A 100%-similarity rename carries no `---`/
            // `+++` hunk headers, so the rename SOURCE (`a/…`) is only
            // recoverable here; dropping it means rewind cannot recreate the
            // moved-from file and the file's content vanishes.
            let mut parts = rest.split_whitespace();
            if let Some(old) = parts.next().and_then(strip_git_prefix) {
                paths.insert(old.to_string());
            }
            if let Some(new) = parts.next().and_then(strip_git_prefix) {
                paths.insert(new.to_string());
            }
            continue;
        }

        // Explicit rename/copy headers are the reliable source of the
        // source/destination paths (and survive paths containing spaces, which
        // the `diff --git` line splits incorrectly).
        if let Some(from) = line
            .strip_prefix("rename from ")
            .or_else(|| line.strip_prefix("copy from "))
        {
            let from = from.trim();
            if !from.is_empty() {
                paths.insert(from.to_string());
            }
            continue;
        }
        if let Some(to) = line
            .strip_prefix("rename to ")
            .or_else(|| line.strip_prefix("copy to "))
        {
            let to = to.trim();
            if !to.is_empty() {
                paths.insert(to.to_string());
            }
            continue;
        }

        if let Some(path) = line
            .strip_prefix("+++ ")
            .or_else(|| line.strip_prefix("--- "))
            .and_then(strip_git_prefix)
            .filter(|path| *path != "/dev/null")
        {
            paths.insert(path.to_string());
        }
    }

    Ok(paths.into_iter().collect())
}

pub(crate) fn strip_git_prefix(path: &str) -> Option<&str> {
    path.strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .or(Some(path))
}

pub(crate) fn validate_relative_path(path: &str) -> Result<()> {
    let path = Path::new(path);

    if path.is_absolute() {
        bail!("patch path must be relative: {}", path.display());
    }

    for component in path.components() {
        match component {
            Component::Normal(_) => {}
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!("patch path escapes workspace: {}", path.display());
            }
        }
    }

    Ok(())
}

pub(crate) fn normalize_workspace_relative_path(path: &Path) -> Result<String> {
    let mut normalized = PathBuf::new();

    for component in path.components() {
        match component {
            Component::Normal(part) => normalized.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!("patch path escapes workspace: {}", path.display());
            }
        }
    }

    Ok(normalized
        .to_string_lossy()
        .trim_start_matches('/')
        .to_string()
        .if_empty("."))
}
