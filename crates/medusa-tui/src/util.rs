use std::{
    env,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use medusa_core::permissions::PermissionMode;
use medusa_core::session::human_bytes;
use medusa_core::tools::TerminalExecResult;

use crate::types::ImageAttachment;

pub(crate) fn terminal_result_output(result: &TerminalExecResult) -> String {
    if result.background {
        return format!(
            "background: running\njob: {}\npid: {}\ncommand: {}",
            result.job_id.as_deref().unwrap_or("unknown"),
            result.pid.unwrap_or(0),
            result.command
        );
    }

    let mut output = format!("exit: {}\n", result.code.unwrap_or(-1));

    if result.stdout.is_empty() {
        output.push_str("stdout: <empty>\n");
    } else {
        output.push_str("stdout:\n");
        output.push_str(&result.stdout);
        if !result.stdout.ends_with('\n') {
            output.push('\n');
        }
    }

    if !result.stderr.is_empty() {
        output.push_str("stderr:\n");
        output.push_str(&result.stderr);
        if !result.stderr.ends_with('\n') {
            output.push('\n');
        }
    }

    output
}

pub(crate) fn parse_exec_command(raw: &str) -> (&str, bool) {
    let trimmed = raw.trim();
    for prefix in ["--background ", "--bg ", "-b "] {
        if let Some(command) = trimmed.strip_prefix(prefix) {
            return (command.trim(), true);
        }
    }
    (trimmed, false)
}

pub(crate) fn queue_count_suffix(count: usize) -> String {
    match count {
        0 | 1 => String::new(),
        count => format!(" · {count} waiting"),
    }
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

pub(crate) fn single_image_path(text: &str) -> Option<PathBuf> {
    let trimmed = text.trim().trim_matches('"').trim_matches('\'');
    if trimmed.lines().count() != 1 {
        return None;
    }
    let path = PathBuf::from(trimmed);
    if !path.is_file() || image_mime_from_path(&path).is_none() {
        return None;
    }
    Some(path)
}

pub(crate) fn image_mime_from_path(path: &Path) -> Option<&'static str> {
    match path
        .extension()?
        .to_string_lossy()
        .to_ascii_lowercase()
        .as_str()
    {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        _ => None,
    }
}

pub(crate) fn image_extension(mime: &str) -> &'static str {
    match mime {
        "image/jpeg" => "jpg",
        _ => "png",
    }
}

pub(crate) fn sanitize_attachment_name(name: &str, extension: &str) -> String {
    let mut safe = name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>();

    if safe.trim_matches('-').is_empty() {
        safe = format!("image.{extension}");
    }
    if Path::new(&safe).extension().is_none() {
        safe.push('.');
        safe.push_str(extension);
    }
    safe
}

pub(crate) fn attachment_timestamp() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

pub(crate) fn attachment_label(attachment: &ImageAttachment) -> String {
    format!(
        "{} {}×{} {}",
        truncate(&attachment.name, 24),
        attachment.width,
        attachment.height,
        human_bytes(attachment.size_bytes)
    )
}

/// Prompt seeded into the composer by /review. Never auto-sent: the user
/// can trim scope or add focus areas before pressing enter.
pub(crate) const REVIEW_PROMPT_TEMPLATE: &str = "\
Review the pending changes in this workspace.

1. Run `git status`, then `git diff` and `git diff --staged`, to see every pending change.
2. Review for correctness bugs first: logic errors, broken edge cases, races, and regressions.
3. Only then look for simplifications: dead code, duplication, and needless complexity.
4. Verify every claim by reading the surrounding code before reporting it — no guesses.
5. Report each finding as file:line with a one-sentence explanation, most severe first.";

/// True when the workspace is inside a git repo with pending changes
/// (staged, unstaged, or untracked — `git status --porcelain` shows all
/// three). The default probe behind /review's App-injectable check.
pub(crate) fn workspace_has_reviewable_diff(workspace: &Path) -> bool {
    let inside_repo = std::process::Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(workspace)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false);
    if !inside_repo {
        return false;
    }
    std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(workspace)
        .output()
        .map(|output| output.status.success() && !output.stdout.is_empty())
        .unwrap_or(false)
}

/// Single-line picker preview: whitespace (including newlines) collapsed,
/// then char-truncated.
pub(crate) fn message_one_liner(content: &str, max_chars: usize) -> String {
    let flat = content.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate(&flat, max_chars)
}

pub(crate) fn truncate(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let truncated = chars.by_ref().take(max_chars).collect::<String>();

    if chars.next().is_some() {
        format!("{truncated}...")
    } else {
        truncated
    }
}

/// Prompt excerpt stored in checkpoint manifests: first line, ≤80 chars.
pub(crate) fn excerpt_for_checkpoint(task: &str) -> String {
    task.lines()
        .next()
        .unwrap_or("")
        .trim()
        .chars()
        .take(80)
        .collect()
}

/// Compact "3m ago"-style label for checkpoint rows.
pub(crate) fn time_ago_ms(created_at_ms: u64) -> String {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let seconds = now_ms.saturating_sub(created_at_ms) / 1000;
    if seconds < 60 {
        "now".to_string()
    } else if seconds < 3_600 {
        format!("{}m ago", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h ago", seconds / 3_600)
    } else {
        format!("{}d ago", seconds / 86_400)
    }
}

pub(crate) fn tool_summary(summary: &str) -> String {
    let summary = summary.trim();
    if summary.is_empty() {
        "tool call".to_string()
    } else {
        compact_one_line(summary, 160)
    }
}

pub(crate) fn compact_one_line(value: &str, max_chars: usize) -> String {
    truncate(
        &value.split_whitespace().collect::<Vec<_>>().join(" "),
        max_chars,
    )
}

pub(crate) fn clean_model_error(error: &str) -> String {
    let compacted = compact_one_line(error, 320);
    let normalized = compacted.replace(['_', '-'], "").to_ascii_lowercase();

    if normalized.contains("serverisoverloaded") || compacted.contains("currently overloaded") {
        return "model overloaded: Our servers are currently overloaded. Please try again later."
            .to_string();
    }

    if compacted.starts_with("model ") && !compacted.contains("{\"response\"") {
        return compacted;
    }

    if let Some(message) = extract_json_error_message(error) {
        return format!("model failed: {}", compact_one_line(&message, 220));
    }

    format!("model failed: {compacted}")
}

pub(crate) fn model_error_status(error: &str) -> &'static str {
    if error.to_ascii_lowercase().contains("overloaded") {
        "model overloaded"
    } else {
        "model failed"
    }
}

pub(crate) fn extract_json_error_message(error: &str) -> Option<String> {
    let message_key = "\"message\":\"";
    let start = error.find(message_key)? + message_key.len();
    let rest = &error[start..];
    let end = rest.find('"')?;
    Some(rest[..end].replace("\\n", " ").replace("\\\"", "\""))
}

pub(crate) fn abbreviate_home(path: &str) -> String {
    let Some(home) = env::var_os("HOME") else {
        return path.to_string();
    };
    let home = home.to_string_lossy();

    path.strip_prefix(home.as_ref())
        .map(|rest| format!("~{rest}"))
        .unwrap_or_else(|| path.to_string())
}

pub(crate) fn command_has_shell_tokens(command: &str) -> bool {
    [
        "\n", "\r", ";", "&&", "||", "|", "&", ">", "<", "`", "$(", "${",
    ]
    .iter()
    .any(|token| command.contains(token))
}

/// Hard-wrap a string to at most `width` columns per line (character-based,
/// no word splitting when a break point exists). Caps the number of lines so a
/// pathological command can't grow the prompt off-screen.
pub(crate) fn wrap_str(text: &str, width: usize) -> Vec<String> {
    const MAX_LINES: usize = 8;
    let width = width.max(8);
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut count = 0usize;
    for ch in text.chars() {
        if count >= width {
            lines.push(std::mem::take(&mut current));
            count = 0;
            if lines.len() == MAX_LINES {
                current.push('…');
                break;
            }
        }
        current.push(ch);
        count += 1;
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// Strip leading `VAR=value` assignments so grant matching sees the program.
pub(crate) fn strip_env_assignments(command: &str) -> &str {
    let mut rest = command.trim_start();
    loop {
        let word_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let word = &rest[..word_end];
        if word.contains('=') && !word.starts_with('-') && !word.is_empty() {
            rest = rest[word_end..].trim_start();
        } else {
            return rest;
        }
    }
}

pub(crate) fn command_matches_grant(command: &str, prefix: &str) -> bool {
    command == prefix
        || command
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with(char::is_whitespace))
}

/// A session edit grant ending in `/` covers a directory subtree; otherwise it
/// is an exact file path (so `Cargo.toml` never grants `Cargo.toml.bak`).
pub(crate) fn edit_grant_matches(grants: &[String], path: &str) -> bool {
    grants.iter().any(|grant| {
        if grant.ends_with('/') {
            path.starts_with(grant.as_str())
        } else {
            path == grant
        }
    })
}

/// Derive the allow-prefix persisted by "always allow": program name, plus
/// the subcommand for multi-command tools, never for compound shell strings.
pub(crate) fn derive_terminal_grant_prefix(command: &str) -> Option<String> {
    let command = command.trim();
    if command.is_empty() || command_has_shell_tokens(command) {
        return None;
    }

    let mut words = command
        .split_whitespace()
        .skip_while(|word| word.contains('=') && !word.starts_with('-'));
    let program = words.next()?;

    // Interpreters and shells take arbitrary code as arguments, so a prefix
    // grant on them ("always allow bash") is a blanket execution grant. These
    // only ever get allow-once, never a persisted/session prefix.
    const INTERPRETER_PROGRAMS: &[&str] = &[
        "sh",
        "bash",
        "zsh",
        "fish",
        "dash",
        "ksh",
        "python",
        "python3",
        "python2",
        "node",
        "deno",
        "ruby",
        "perl",
        "php",
        "lua",
        "Rscript",
        "osascript",
        "env",
        "eval",
        "exec",
        "xargs",
        "nohup",
        "time",
        "sudo",
        "doas",
        "ssh",
        "docker",
        "kubectl",
    ];
    if INTERPRETER_PROGRAMS.contains(&program) {
        return None;
    }

    const SUBCOMMAND_PROGRAMS: &[&str] = &[
        "git", "cargo", "npm", "pnpm", "yarn", "bun", "make", "go", "pip", "pip3", "uv", "just",
    ];
    if !SUBCOMMAND_PROGRAMS.contains(&program) {
        return Some(program.to_string());
    }

    let subcommand = words.next().filter(|word| !word.starts_with('-'));
    match subcommand {
        Some("run") => {
            let script = words.next().filter(|word| !word.starts_with('-'));
            match script {
                Some(script) => Some(format!("{program} run {script}")),
                None => Some(format!("{program} run")),
            }
        }
        Some(sub) => Some(format!("{program} {sub}")),
        None => Some(program.to_string()),
    }
}

pub(crate) fn permission_context_text(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Open => {
            "Medusa permission mode: open. Normal workspace inspection, terminal commands, and file mutations are available subject to workspace boundaries."
        }
        PermissionMode::Guarded => {
            "Medusa permission mode: guarded. Workspace inspection is available. Terminal commands and file mutations are allowed unless blocked by Medusa's guarded permission policy. Mention guarded mode when a command or edit is blocked."
        }
        PermissionMode::Ask => {
            "Medusa permission mode: ask. Safe inspection commands run freely; mutating terminal commands and file edits pause for the user's interactive approval. If a tool result says it was denied by the user, respect the decision — do not retry the same operation; adjust the approach or ask the user."
        }
        PermissionMode::Readonly => {
            "Medusa permission mode: readonly. Reading, listing, searching, and safe inspection commands are allowed. File mutation tools are unavailable for this turn, and write-like shell commands are blocked by policy. If the user asks for changes, explain that this session is in readonly mode and offer a plan or ask them to switch permissions."
        }
    }
}
