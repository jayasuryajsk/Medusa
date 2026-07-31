use super::*;

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
