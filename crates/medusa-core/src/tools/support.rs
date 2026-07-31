use std::{
    collections::BTreeSet,
    ffi::OsStr,
    fs,
    io::Write,
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    time::Instant,
};

use color_eyre::eyre::{Result, WrapErr, bail};

use crate::persistence::atomic_write;

use super::runtime::ToolRuntime;
use super::types::*;

pub(crate) fn normalize_plan_status(status: &str) -> Option<&'static str> {
    match status
        .trim()
        .to_ascii_lowercase()
        .replace(['_', '-'], " ")
        .as_str()
    {
        "pending" | "todo" | "queued" => Some("pending"),
        "active" | "in progress" | "current" | "doing" => Some("active"),
        "done" | "complete" | "completed" | "succeeded" => Some("done"),
        "blocked" | "failed" | "stuck" => Some("blocked"),
        _ => None,
    }
}

pub(crate) fn normalize_decision_kind(kind: &str) -> &'static str {
    match kind
        .trim()
        .to_ascii_lowercase()
        .replace(['_', '-'], " ")
        .as_str()
    {
        "text" | "free text" | "freeform" | "free form" => "text",
        "choice" | "single choice" | "single" => "choice",
        _ => "choice",
    }
}

pub(crate) fn sanitize_decision_id(id: &str) -> Option<String> {
    let id = id
        .trim()
        .chars()
        .filter_map(|ch| {
            if ch.is_ascii_alphanumeric() {
                Some(ch.to_ascii_lowercase())
            } else if matches!(ch, '-' | '_' | ' ') {
                Some('_')
            } else {
                None
            }
        })
        .collect::<String>()
        .trim_matches('_')
        .chars()
        .take(48)
        .collect::<String>();
    (!id.is_empty()).then_some(id)
}

pub(crate) fn run_explore_probe(
    tools: &ToolRuntime,
    index: usize,
    probe: ExploreProbe,
) -> ExploreProbeResult {
    let started = Instant::now();
    let kind = probe.kind.as_str().to_string();
    let label = explore_probe_label(&probe);
    let result = match probe.kind {
        ExploreProbeKind::List => {
            let request = FsListRequest {
                path: probe.path,
                depth: probe.depth,
                max_entries: probe.max_entries.or(probe.max_results),
            };
            tools
                .fs_list(request)
                .map(|result| summarize_list_evidence(&result))
        }
        ExploreProbeKind::Search => {
            let query = probe.query.unwrap_or_default();
            let request = FileSearchRequest {
                query,
                path: probe.path,
                depth: probe.depth,
                max_results: probe.max_results,
                case_sensitive: probe.case_sensitive,
                include: None,
            };
            tools
                .file_search(request)
                .map(|result| summarize_search_evidence(&result))
        }
        ExploreProbeKind::Read => {
            let request = FileReadRequest {
                paths: probe.paths,
                start_line: probe.start_line,
                end_line: probe.end_line,
            };
            tools
                .file_read(request)
                .map(|result| summarize_read_evidence(&result))
        }
        ExploreProbeKind::Terminal => {
            let command = probe.command.unwrap_or_default();
            validate_explore_terminal_command(&command, tools.workspace())
                .and_then(|_| {
                    tools.terminal_exec_gated(
                        TerminalExecRequest {
                            command,
                            cwd: probe.cwd,
                            background: false,
                            unsandboxed: false,
                        },
                        true,
                    )
                })
                .map(|result| summarize_terminal_evidence(&result))
        }
    };

    match result {
        Ok(output) => ExploreProbeResult {
            index,
            kind,
            label,
            failed: false,
            output,
            elapsed_ms: started.elapsed().as_millis(),
        },
        Err(error) => ExploreProbeResult {
            index,
            kind,
            label,
            failed: true,
            output: format!("error: {error}"),
            elapsed_ms: started.elapsed().as_millis(),
        },
    }
}

pub(crate) fn explore_probe_label(probe: &ExploreProbe) -> String {
    match probe.kind {
        ExploreProbeKind::List => probe
            .path
            .as_ref()
            .map(|path| format!("list {}", path.display()))
            .unwrap_or_else(|| "list workspace".to_string()),
        ExploreProbeKind::Search => probe
            .query
            .as_deref()
            .map(|query| format!("search {query:?}"))
            .unwrap_or_else(|| "search files".to_string()),
        ExploreProbeKind::Read => {
            if probe.paths.len() == 1 {
                format!("read {}", probe.paths[0].display())
            } else {
                format!("read {} files", probe.paths.len())
            }
        }
        ExploreProbeKind::Terminal => probe
            .command
            .as_deref()
            .map(|command| format!("$ {command}"))
            .unwrap_or_else(|| "run read-only command".to_string()),
    }
}

pub(crate) fn summarize_list_evidence(result: &FsListResult) -> String {
    let mut output = format!(
        "root: {}{}\nentries: {}\n",
        result.root,
        if result.truncated { " (truncated)" } else { "" },
        result.entries.len()
    );
    for entry in result.entries.iter().take(40) {
        output.push_str(&format!(
            "{}{} {}\n",
            "  ".repeat(entry.depth),
            entry.kind,
            entry.path
        ));
    }
    if result.entries.len() > 40 {
        output.push_str(&format!("... {} more entries\n", result.entries.len() - 40));
    }
    compact_text(&output, 6000)
}

pub(crate) fn summarize_search_evidence(result: &FileSearchResult) -> String {
    let mut output = format!(
        "query: {}\nsearched files: {}\nmatches: {}{}\n",
        result.query,
        result.searched_files,
        result.matches.len(),
        if result.truncated { " (truncated)" } else { "" }
    );
    for hit in result.matches.iter().take(40) {
        output.push_str(&format!("{}:{}: {}\n", hit.path, hit.line, hit.text));
    }
    if result.matches.len() > 40 {
        output.push_str(&format!("... {} more matches\n", result.matches.len() - 40));
    }
    compact_text(&output, 7000)
}

pub(crate) fn summarize_read_evidence(result: &FileReadResult) -> String {
    let mut output = format!("read files: {}\n", result.files.len());
    for file in &result.files {
        output.push_str(&format!(
            "{}:{}-{} / {} lines{}\n",
            file.path,
            file.start_line,
            file.end_line,
            file.total_lines,
            if file.truncated { " (truncated)" } else { "" }
        ));
        for line in file.lines.iter().take(120) {
            output.push_str(&format!("{:>5} | {}\n", line.number, line.text));
        }
        if file.lines.len() > 120 {
            output.push_str(&format!("... {} more lines\n", file.lines.len() - 120));
        }
    }
    compact_text(&output, 10_000)
}

pub(crate) fn summarize_terminal_evidence(result: &TerminalExecResult) -> String {
    let mut output = format!("exit: {}\n", result.code.unwrap_or(-1));
    if !result.stdout.trim().is_empty() {
        output.push_str("stdout:\n");
        output.push_str(&result.stdout);
        if !result.stdout.ends_with('\n') {
            output.push('\n');
        }
    }
    if !result.stderr.trim().is_empty() {
        output.push_str("stderr:\n");
        output.push_str(&result.stderr);
        if !result.stderr.ends_with('\n') {
            output.push('\n');
        }
    }
    if result.stdout.trim().is_empty() && result.stderr.trim().is_empty() {
        output.push_str("output: <empty>\n");
    }
    compact_text(&output, 6000)
}

pub(crate) fn validate_explore_terminal_command(command: &str, workspace: &Path) -> Result<()> {
    let command = command.trim();
    if command.is_empty() {
        bail!("explore terminal probe requires command");
    }

    let forbidden_fragments = [
        "\n",
        "\r",
        ";",
        "|",
        "&",
        ">",
        "<",
        "`",
        // Bare `$` blocks all env-var expansion ($HOME, $file) in explore
        // probes, not only command substitution — the shell would expand it to
        // an unconfined path the preapproved fast-lane must never read.
        "$",
        " rm ",
        " rm -",
        "mv ",
        "cp ",
        "touch ",
        "mkdir ",
        "rmdir ",
        "chmod ",
        "chown ",
        "sed -i",
        "perl -pi",
        "tee ",
        "npm install",
        "pnpm install",
        "yarn add",
        "cargo add",
        "git reset",
        "git checkout",
        "git clean",
        "git apply",
        "git commit",
        "git push",
    ];
    let padded = format!(" {command} ");
    for fragment in forbidden_fragments {
        if command.contains(fragment) || padded.contains(fragment) {
            bail!(
                "terminal probe is not read-only enough for explore_batch: contains `{fragment}`"
            );
        }
    }

    // `find` can execute arbitrary programs; only allow it without action
    // predicates entirely.
    if command.starts_with("find ") {
        for action in [
            " -delete",
            " -exec",
            " -execdir",
            " -ok",
            " -okdir",
            " -fprint",
            " -fprintf",
            " -fls",
        ] {
            if command.contains(action) {
                bail!("terminal probe is not read-only enough for explore_batch: find{action}");
            }
        }
    }
    if command.starts_with("cargo fmt") && !command.starts_with("cargo fmt --check") {
        bail!(
            "terminal probe is not read-only enough for explore_batch: cargo fmt must use --check"
        );
    }

    let allowed_prefixes = [
        "pwd",
        "ls",
        "cat ",
        "sed -n ",
        "rg ",
        "grep ",
        "find ",
        "git status",
        "git diff",
        "git log",
        "cargo check",
        "cargo test",
        "cargo clippy",
        "cargo fmt --check",
        "npm test",
        "npm run test",
        "pnpm test",
        "pnpm run test",
        "yarn test",
    ];
    // Require a word boundary after the prefix so `git diff` cannot admit
    // `git difftool -x <cmd>`, nor `ls` admit `lsof`.
    if !allowed_prefixes.iter().any(|prefix| {
        let prefix = prefix.trim_end();
        command == prefix
            || command
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.starts_with(char::is_whitespace))
    }) {
        bail!("terminal probe is not in the explore_batch read-only allowlist");
    }

    // Explore probes are the *preapproved* fast lane — they never see a human
    // click — so they must stay inside the workspace. A read of an absolute or
    // escaping path (`cat /Users/x/.ssh/id_rsa`, `head ../../etc/passwd`) has
    // no business on this lane; force it onto the normal gated terminal path.
    let escaping = command_paths_outside_workspace(command, workspace);
    if let Some(token) = escaping.first() {
        bail!(
            "explore terminal probe references a path outside the workspace (`{token}`); \
             out-of-workspace reads must go through the gated terminal, not explore_batch"
        );
    }

    Ok(())
}

pub(crate) fn validate_read_only_terminal_command(command: &str, workspace: &Path) -> Result<()> {
    validate_explore_terminal_command(command, workspace)
}

/// Best-effort scan of a shell command's whitespace-split tokens for path
/// arguments that resolve OUTSIDE `workspace`. Returns the offending raw
/// tokens (empty when every referenced path stays inside the workspace).
///
/// A token is treated as a filesystem path only when it *looks* like one: it
/// is absolute (`/…`), home-relative (`~…`/`~user`), or contains a path
/// separator (`foo/bar`, `../x`). Bare words (`Cargo.toml`, `src`), flags
/// (`-n`, `--color`), and numeric args are never paths — they can only resolve
/// inside the workspace — so they are ignored to avoid over-prompting on
/// ordinary in-workspace reads.
///
/// This is deliberately NOT a sandbox. It splits on ASCII whitespace and
/// cannot see through command substitution (`$(…)`, backticks), variable
/// expansion (`$HOME`, `${x}`), globbing, or quoted whitespace inside a single
/// argument. Callers that must block those shapes reject shell-control tokens
/// separately (see `validate_explore_terminal_command` and
/// `permissions::contains_shell_control_tokens`). Its only job is to keep an
/// otherwise-auto-approved read from silently touching an absolute or escaping
/// path; over-flagging is acceptable, silent escapes are not.
pub(crate) fn command_paths_outside_workspace(command: &str, workspace: &Path) -> Vec<String> {
    let mut escaping = Vec::new();
    for raw in command.split_whitespace() {
        // Peel a flag's `=value` (`--file=/etc/passwd`, `--output=../x`) so the
        // value is still inspected; a bare flag (`-n`, `--color`) has no path.
        let candidate = if let Some(rest) = raw.strip_prefix('-') {
            match rest.split_once('=') {
                Some((_, value)) => value,
                None => continue,
            }
        } else {
            raw
        };
        let candidate = strip_matching_quotes(candidate);
        if candidate.is_empty() {
            continue;
        }
        if token_escapes_workspace(candidate, workspace) {
            escaping.push(raw.to_string());
        }
    }
    escaping
}

/// Whether a single already-dequoted token references a path outside
/// `workspace`. Bare words (no separator, not `/`- or `~`-prefixed) can only
/// live inside the workspace, so they are never escapes.
pub(crate) fn token_escapes_workspace(token: &str, workspace: &Path) -> bool {
    // `~` / `~user` always denote a home directory; we cannot expand it without
    // reading the environment, so treat any home-relative token as an escape
    // (conservative over-prompt rather than a silent out-of-tree read).
    if token.starts_with('~') {
        return true;
    }
    let path = Path::new(token);
    let resolved = if path.is_absolute() {
        lexically_normalize(path)
    } else if token.contains('/') {
        lexically_normalize(&workspace.join(path))
    } else {
        // Bare word (`Cargo.toml`, `src`, `1,5p`): resolves inside the
        // workspace, never an escape.
        return false;
    };
    !resolved.starts_with(workspace)
}

/// Resolve `.`/`..` components purely lexically, without touching the
/// filesystem (so non-existent and escaping paths are handled the same, and
/// the function stays a testable pure fn). A leading `..` that would rise above
/// the root is kept, so the result can no longer carry an interior `workspace`
/// prefix.
pub(crate) fn lexically_normalize(path: &Path) -> PathBuf {
    let mut out: Vec<Component> = Vec::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(out.last(), Some(Component::Normal(_))) {
                    out.pop();
                } else {
                    out.push(comp);
                }
            }
            other => out.push(other),
        }
    }
    out.iter().collect()
}

/// Strip one pair of matching surrounding ASCII quotes from a whitespace-split
/// token (`"../x"` → `../x`). Only handles a quote wrapping the whole token —
/// this is a heuristic, not a shell lexer.
pub(crate) fn strip_matching_quotes(token: &str) -> &str {
    let bytes = token.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return &token[1..token.len() - 1];
        }
    }
    token
}

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

pub(crate) fn sorted_read_dir(path: &Path) -> Result<Vec<fs::DirEntry>> {
    let mut entries = fs::read_dir(path)
        .wrap_err_with(|| format!("failed to list {}", path.display()))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .wrap_err_with(|| format!("failed to read directory entry in {}", path.display()))?;
    entries.sort_by_key(|entry| entry.path());
    Ok(entries)
}

pub(crate) fn should_skip_dir(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(OsStr::to_str),
        Some(
            ".git"
                | ".hg"
                | ".svn"
                | ".medusa"
                | ".next"
                | ".turbo"
                | ".venv"
                | "__pycache__"
                | "build"
                | "coverage"
                | "dist"
                | "node_modules"
                | "target"
        )
    )
}

pub(crate) trait IfEmpty {
    fn if_empty(self, fallback: &str) -> String;
}

impl IfEmpty for String {
    fn if_empty(self, fallback: &str) -> String {
        if self.is_empty() {
            fallback.to_string()
        } else {
            self
        }
    }
}
