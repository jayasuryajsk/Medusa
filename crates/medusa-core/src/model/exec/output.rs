use super::*;

pub(crate) fn compact_tool_context_output(call: &ToolCall, execution: &ToolExecution) -> String {
    if execution.failed {
        return compact(&execution.output, 6000);
    }

    match call.name.as_str() {
        "file_read" | "file_search" | "semantic_search" | "file_glob" | "fs_list"
        | "explore_batch" | "web_fetch" | "web_search" => compact(&execution.output, 20_000),
        "terminal_exec" => compact_terminal_context_output(&execution.output, 6000),
        "workflow_run" => compact(&execution.output, 8000),
        // Short confirmations only: the model already produced the edit content,
        // so echoing a diff back would just duplicate tokens in context. The
        // verify: block DOES go back — breakage feedback is the whole point.
        "file_edit" => {
            let (base, verify) = split_verify_section(&execution.output);
            let mut context = summarize_file_edit_output(base);
            if let Some(block) = verify {
                context.push('\n');
                context.push_str(block);
            }
            context
        }
        "file_patch" => {
            let (base, verify) = split_verify_section(&execution.output);
            let mut context = summarize_file_patch_output(base);
            if let Some(block) = verify {
                context.push('\n');
                context.push_str(block);
            }
            context
        }
        "task_update" => execution.output.clone(),
        "plan_update" => execution.output.clone(),
        "decision_request" => execution.output.clone(),
        "question" => execution.output.clone(),
        // MCP results can be large documents; cap harder than generic tools
        // but keep enough for the model to actually use them.
        name if name.starts_with("mcp_") => compact(&execution.output, 8000),
        _ => compact(&execution.output, 4000),
    }
}

pub(crate) fn summarize_tool_result(call: &ToolCall, execution: &ToolExecution) -> String {
    if execution.failed {
        return format!("failed • {}", compact(&execution.output, 500));
    }

    match call.name.as_str() {
        "file_read" => summarize_file_read_output(&execution.output),
        "file_search" => summarize_file_search_output(&execution.output),
        "semantic_search" => summarize_semantic_search_output(&execution.output),
        "file_glob" => summarize_file_glob_output(&execution.output),
        "fs_list" => summarize_fs_list_output(&execution.output),
        "explore_batch" => summarize_explore_batch_output(&execution.output),
        "web_fetch" => summarize_web_fetch_output(&execution.output),
        "web_search" => summarize_web_search_output(&execution.output),
        "terminal_exec" => summarize_terminal_output(&execution.output),
        "file_edit" => {
            let (base, verify) = split_verify_section(&execution.output);
            compose_mutation_summary(
                summarize_file_edit_output(base),
                verify,
                file_edit_display_diff(call),
            )
        }
        "file_patch" => {
            let (base, verify) = split_verify_section(&execution.output);
            compose_mutation_summary(
                summarize_file_patch_output(base),
                verify,
                file_patch_display_diff(call),
            )
        }
        "workflow_run" => execution
            .output
            .lines()
            .next()
            .unwrap_or("workflow completed")
            .to_string(),
        "task_update" => execution.output.clone(),
        "plan_update" => execution.output.clone(),
        "decision_request" => execution.output.clone(),
        "question" => execution.output.clone(),
        _ => compact(&execution.output, 500),
    }
}

/// Transcript layout for a mutation: verify status rides the headline,
/// failure details come before the diff (breakage first), diff last.
fn compose_mutation_summary(
    base_summary: String,
    verify: Option<&str>,
    diff: Option<String>,
) -> String {
    let mut summary = base_summary;
    let mut details = None;
    if let Some(block) = verify {
        let mut lines = block.lines();
        if let Some(status) = lines.next() {
            summary.push_str(" · ");
            summary.push_str(status);
        }
        let rest = lines.collect::<Vec<_>>().join("\n");
        if !rest.trim().is_empty() {
            details = Some(rest);
        }
    }
    if let Some(details) = details {
        summary.push('\n');
        summary.push_str(&details);
    }
    if let Some(diff) = diff {
        summary.push('\n');
        summary.push_str(&diff);
    }
    summary
}

fn summarize_file_edit_output(output: &str) -> String {
    output
        .strip_prefix("edited files:\n")
        .map(|rest| {
            let path = rest.lines().next().unwrap_or("file");
            let replacements = rest
                .lines()
                .find_map(|line| line.strip_prefix("replacements: "))
                .unwrap_or("1");
            format!(
                "edited {path} ({replacements} replacement{})",
                if replacements == "1" { "" } else { "s" }
            )
        })
        .unwrap_or_else(|| compact(output, 500))
}

fn summarize_file_patch_output(output: &str) -> String {
    output
        .strip_prefix("patched files:\n")
        .map(|files| format!("patched {}", files.lines().collect::<Vec<_>>().join(", ")))
        .unwrap_or_else(|| compact(output, 500))
}

/// Maximum diff lines shown in the transcript before folding the rest.
const DISPLAY_DIFF_MAX_LINES: usize = 60;

/// Unified diff of a file_edit's old/new strings, for the transcript only —
/// never sent back to the model.
fn file_edit_display_diff(call: &ToolCall) -> Option<String> {
    let args: Value = serde_json::from_str(&call.arguments).ok()?;
    let old = string_arg(&args, "oldString", "old_string").unwrap_or_default();
    let new = string_arg(&args, "newString", "new_string")?;
    let diff = similar::TextDiff::from_lines(old, new);

    let mut lines = Vec::new();
    for hunk in diff.unified_diff().context_radius(2).iter_hunks() {
        for change in hunk.iter_changes() {
            let sign = match change.tag() {
                similar::ChangeTag::Insert => '+',
                similar::ChangeTag::Delete => '-',
                similar::ChangeTag::Equal => ' ',
            };
            lines.push(format!("{sign} {}", change.value().trim_end_matches('\n')));
        }
    }
    if lines.is_empty() {
        return None;
    }
    Some(cap_display_diff(lines))
}

/// The patch body a file_patch call applied, cleaned up for the transcript.
fn file_patch_display_diff(call: &ToolCall) -> Option<String> {
    let args: Value = serde_json::from_str(&call.arguments).ok()?;
    let diff = args.get("diff").and_then(Value::as_str)?;
    let lines: Vec<String> = diff
        .lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            !trimmed.is_empty()
                && !trimmed.starts_with("```")
                && !trimmed.starts_with("diff --git")
                && !trimmed.starts_with("index ")
        })
        .map(|line| {
            // Match file_edit diff formatting: a space after the sign column.
            match line.as_bytes().first() {
                Some(b'+') if !line.starts_with("+++") => format!("+ {}", &line[1..]),
                Some(b'-') if !line.starts_with("---") => format!("- {}", &line[1..]),
                _ => line.to_string(),
            }
        })
        .collect();
    if lines.is_empty() {
        return None;
    }
    Some(cap_display_diff(lines))
}

fn cap_display_diff(mut lines: Vec<String>) -> String {
    if lines.len() > DISPLAY_DIFF_MAX_LINES {
        let hidden = lines.len() - DISPLAY_DIFF_MAX_LINES;
        lines.truncate(DISPLAY_DIFF_MAX_LINES);
        lines.push(format!("… +{hidden} more diff lines"));
    }
    lines.join("\n")
}

fn summarize_file_read_output(output: &str) -> String {
    let count = output
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("read files: "))
        .unwrap_or("0");
    let first_path = output
        .lines()
        .nth(1)
        .and_then(|line| line.split(':').next())
        .unwrap_or("files");
    format!("read {count} • {}", compact(first_path, 160))
}

fn summarize_file_search_output(output: &str) -> String {
    let query = output
        .lines()
        .find_map(|line| line.strip_prefix("query: "))
        .unwrap_or("");
    let matches = output
        .lines()
        .find_map(|line| line.strip_prefix("matches: "))
        .unwrap_or("0");
    if query.is_empty() {
        format!("matches {matches}")
    } else {
        format!("matches {matches} • {query:?}")
    }
}

fn summarize_semantic_search_output(output: &str) -> String {
    let query = output
        .lines()
        .find_map(|line| line.strip_prefix("query: "))
        .unwrap_or("");
    let matches = output
        .lines()
        .find_map(|line| line.strip_prefix("matches: "))
        .unwrap_or("0");
    format!("found {matches} files by meaning • {query:?}")
}

fn summarize_file_glob_output(output: &str) -> String {
    let pattern = output
        .lines()
        .find_map(|line| line.strip_prefix("pattern: "))
        .unwrap_or("");
    let matches = output
        .lines()
        .find_map(|line| line.strip_prefix("matches: "))
        .unwrap_or("0");
    if pattern.is_empty() {
        format!("matched {matches}")
    } else {
        format!("matched {matches} • {pattern}")
    }
}

fn summarize_fs_list_output(output: &str) -> String {
    let root = output
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("root: "))
        .unwrap_or(".");
    let entries = output
        .lines()
        .skip(1)
        .filter(|line| !line.trim().is_empty())
        .count();
    format!("listed {entries} • {}", compact(root, 160))
}

fn summarize_explore_batch_output(output: &str) -> String {
    let probes = output
        .lines()
        .find_map(|line| line.strip_prefix("probes: "))
        .unwrap_or("completed");
    format!("evidence • {}", compact(probes, 180))
}

fn summarize_web_fetch_output(output: &str) -> String {
    let url = output
        .lines()
        .find_map(|line| line.strip_prefix("url: "))
        .unwrap_or("");
    let status = output
        .lines()
        .find_map(|line| line.strip_prefix("status: "))
        .unwrap_or("fetched");
    if url.is_empty() {
        format!("fetched • {status}")
    } else {
        format!("{status} • {}", compact(url, 160))
    }
}

fn summarize_web_search_output(output: &str) -> String {
    let query = output
        .lines()
        .find_map(|line| line.strip_prefix("query: "))
        .unwrap_or("");
    let results = output
        .lines()
        .find_map(|line| line.strip_prefix("results: "))
        .unwrap_or("0");
    if query.is_empty() {
        format!("results {results}")
    } else {
        format!("results {results} • {}", compact(query, 120))
    }
}

fn summarize_terminal_output(output: &str) -> String {
    let exit = output
        .lines()
        .next()
        .unwrap_or("exit: unknown")
        .trim()
        .to_string();

    let stdout = section_after(output, "stdout:", Some("stderr:")).unwrap_or_default();
    let stderr = section_after(output, "stderr:", None).unwrap_or_default();
    let combined = if stderr.trim().is_empty() {
        &stdout
    } else {
        &stderr
    };

    let preview =
        summarize_command_text(&strip_ansi(combined)).unwrap_or_else(|| "no output".to_string());
    let mut result = format!("{exit} • {}", compact(&preview, 240));
    for note in terminal_header_notes(output) {
        result.push('\n');
        result.push_str(note);
    }

    // Tail of the raw output for the transcript's expanded view. ANSI codes
    // stay intact here — the UI renders them as colors; the model context
    // path strips them instead.
    let tail = terminal_tail_lines(combined, 40);
    if tail.len() > 1 || tail.first().map(String::as_str) != Some(preview.as_str()) {
        for line in tail {
            result.push('\n');
            result.push_str(&line);
        }
    }
    result
}

/// Harness-generated note lines (`sandbox:`, `hint:`, `note:`) that
/// execute_terminal_exec places between the exit line and the stdout marker.
/// They must survive both the transcript summary and the model-context
/// compaction, which otherwise only keep the stdout/stderr sections.
fn terminal_header_notes(output: &str) -> Vec<&str> {
    output
        .lines()
        .skip(1)
        .take_while(|line| !line.starts_with("stdout:"))
        .filter(|line| {
            line.starts_with("sandbox:") || line.starts_with("hint:") || line.starts_with("note:")
        })
        .collect()
}

/// The last `limit` non-empty output lines, preserving indentation.
fn terminal_tail_lines(text: &str, limit: usize) -> Vec<String> {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty() && *line != "<empty>")
        .collect();
    lines
        .iter()
        .skip(lines.len().saturating_sub(limit))
        .map(ToString::to_string)
        .collect()
}

/// Remove ANSI escape sequences (colors, cursor movement, OSC titles).
pub(crate) fn strip_ansi(text: &str) -> String {
    static ANSI: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = ANSI.get_or_init(|| {
        regex::Regex::new(r"\x1b(?:\[[0-9;?]*[ -/]*[@-~]|\][^\x07\x1b]*(?:\x07|\x1b\\)|[@-Z\\-_])")
            .expect("ANSI regex compiles")
    });
    re.replace_all(text, "").into_owned()
}

fn compact_terminal_context_output(output: &str, max_chars: usize) -> String {
    let exit = output.lines().next().unwrap_or("exit: unknown").trim();
    let stdout = section_after(output, "stdout:", Some("stderr:")).unwrap_or_default();
    let stderr = section_after(output, "stderr:", None).unwrap_or_default();
    let source = if stderr.trim().is_empty() {
        &stdout
    } else {
        &stderr
    };

    let mut result = String::new();
    result.push_str(exit);
    for note in terminal_header_notes(output) {
        result.push('\n');
        result.push_str(note);
    }
    if let Some(summary) = summarize_command_text(source) {
        result.push_str("\nsummary: ");
        result.push_str(&summary);
    }

    let important = important_output_lines(source, 80);
    if !important.is_empty() {
        result.push_str("\npreview:\n");
        result.push_str(&important.join("\n"));
    }

    // Escape codes are pure token waste in model context.
    compact(&strip_ansi(&result), max_chars)
}

fn section_after(output: &str, marker: &str, until: Option<&str>) -> Option<String> {
    let mut in_section = false;
    let mut lines = Vec::new();
    for line in output.lines() {
        if line == marker {
            in_section = true;
            continue;
        }
        if in_section && until.is_some_and(|end| line == end) {
            break;
        }
        if in_section {
            lines.push(line);
        }
    }

    if lines.is_empty() || lines == ["<empty>"] {
        None
    } else {
        Some(lines.join("\n"))
    }
}

fn summarize_command_text(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed == "<empty>" {
        return None;
    }

    for line in trimmed
        .lines()
        .rev()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        if line.contains(" passed") && (line.contains("test result:") || line.contains("passed;")) {
            return Some(line.to_string());
        }
        if line.contains("error") || line.contains("failed") || line.contains("panicked") {
            return Some(line.to_string());
        }
    }

    let count = trimmed
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count();
    match count {
        0 => None,
        1 => Some(
            trimmed
                .lines()
                .next()
                .unwrap_or_default()
                .trim()
                .to_string(),
        ),
        _ => Some(format!("{count} lines")),
    }
}

fn important_output_lines(text: &str, limit: usize) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && *line != "<empty>")
        .filter(|line| {
            line.starts_with("error")
                || line.starts_with("warning")
                || line.contains("failed")
                || line.contains("panicked")
                || line.contains("test result:")
                || line.contains(" passed")
        })
        .take(8)
        .map(|line| compact(line, limit))
        .collect()
}
