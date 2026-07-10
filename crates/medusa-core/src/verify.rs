//! Post-edit verification: after the model's last successful file mutation in
//! a turn, run one cheap project-aware check (`cargo check`, `go build`,
//! `tsc --noEmit`, `python -m py_compile`) and feed a compact pass/fail
//! signal back into the tool result, so the model sees breakage immediately
//! instead of discovering it turns later.
//!
//! Verification is harness-initiated (not a model tool call), best-effort,
//! and silent when no verifier applies. Disable with `MEDUSA_VERIFY=off`.

use std::{
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

use crate::cancel::CancelToken;

const DEFAULT_TIMEOUT_SECS: u64 = 90;
const MAX_DETAIL_LINES: usize = 15;
const MAX_DETAIL_CHARS: usize = 2_400;

/// Formatted `verify:` block to append to a mutation's tool output, or None
/// when verification is disabled, the turn was cancelled, no verifier
/// applies, or the changed files aren't relevant to the detected project
/// type.
pub fn verify_after_mutation(
    workspace: &Path,
    changed_files: &[String],
    cancel: &CancelToken,
) -> Option<String> {
    if verification_disabled() || cancel.is_cancelled() {
        return None;
    }
    let verifier = detect_verifier(workspace, changed_files)?;
    Some(run_verifier(workspace, &verifier, cancel))
}

fn verification_disabled() -> bool {
    std::env::var("MEDUSA_VERIFY")
        .map(|value| matches!(value.trim(), "off" | "0" | "false" | "no"))
        .unwrap_or(false)
}

fn verify_timeout() -> Duration {
    let seconds = std::env::var("MEDUSA_VERIFY_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_TIMEOUT_SECS);
    Duration::from_secs(seconds)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Verifier {
    /// Short human label, e.g. "cargo check".
    pub(crate) label: String,
    pub(crate) program: PathBuf,
    pub(crate) args: Vec<String>,
}

fn changed_with_extension(changed_files: &[String], extensions: &[&str]) -> bool {
    changed_files.iter().any(|file| {
        let file = file.trim();
        extensions.iter().any(|extension| {
            file.ends_with(extension)
                || Path::new(file)
                    .file_name()
                    .is_some_and(|name| name == *extension)
        })
    })
}

/// Pick the cheapest meaningful check for this workspace, gated on the
/// changed files actually being relevant to it (editing a README must not
/// trigger a build).
pub(crate) fn detect_verifier(workspace: &Path, changed_files: &[String]) -> Option<Verifier> {
    if changed_files.is_empty() {
        return None;
    }

    if workspace.join("Cargo.toml").is_file()
        && changed_with_extension(changed_files, &[".rs", "Cargo.toml"])
    {
        return Some(Verifier {
            label: "cargo check".to_string(),
            program: PathBuf::from("cargo"),
            args: vec![
                "check".to_string(),
                "--workspace".to_string(),
                "--quiet".to_string(),
                "--message-format=short".to_string(),
            ],
        });
    }

    if workspace.join("go.mod").is_file()
        && changed_with_extension(changed_files, &[".go", "go.mod"])
    {
        return Some(Verifier {
            label: "go build".to_string(),
            program: PathBuf::from("go"),
            args: vec!["build".to_string(), "./...".to_string()],
        });
    }

    let local_tsc = workspace.join("node_modules/.bin/tsc");
    if workspace.join("tsconfig.json").is_file()
        && local_tsc.is_file()
        && changed_with_extension(changed_files, &[".ts", ".tsx", ".mts", ".cts"])
    {
        return Some(Verifier {
            label: "tsc --noEmit".to_string(),
            program: local_tsc,
            args: vec!["--noEmit".to_string()],
        });
    }

    let python_files: Vec<String> = changed_files
        .iter()
        .map(|file| file.trim().to_string())
        .filter(|file| file.ends_with(".py"))
        .collect();
    if !python_files.is_empty() {
        let mut args = vec!["-m".to_string(), "py_compile".to_string()];
        args.extend(python_files);
        return Some(Verifier {
            label: "python py_compile".to_string(),
            program: PathBuf::from("python3"),
            args,
        });
    }

    None
}

fn run_verifier(workspace: &Path, verifier: &Verifier, cancel: &CancelToken) -> String {
    let timeout = verify_timeout();
    let started = Instant::now();
    let mut command = Command::new(&verifier.program);
    command.args(&verifier.args).current_dir(workspace);

    match crate::proc::run_command(command, Some(timeout), cancel) {
        Err(_) => {
            // Tool missing or unspawnable — verification is best-effort.
            format!("verify: {} unavailable (skipped)", verifier.label)
        }
        Ok(outcome) => {
            let elapsed = format_elapsed(started.elapsed());
            if outcome.cancelled {
                return format!("verify: {} cancelled (turn interrupted)", verifier.label);
            }
            if outcome.timed_out {
                return format!(
                    "verify: {} timed out after {}s (run it manually if needed)",
                    verifier.label,
                    timeout.as_secs()
                );
            }
            if outcome.success {
                format!("verify: {} ok ({elapsed})", verifier.label)
            } else {
                let details = failure_details(&outcome.stdout, &outcome.stderr);
                if details.is_empty() {
                    format!("verify: {} FAILED ({elapsed})", verifier.label)
                } else {
                    format!("verify: {} FAILED ({elapsed})\n{details}", verifier.label)
                }
            }
        }
    }
}

fn format_elapsed(elapsed: Duration) -> String {
    if elapsed.as_secs() >= 10 {
        format!("{}s", elapsed.as_secs())
    } else {
        format!("{:.1}s", elapsed.as_secs_f64())
    }
}

/// Compact failure output: prefer error lines, cap size hard so a broken
/// build can't flood the transcript or the model context.
fn failure_details(stdout: &str, stderr: &str) -> String {
    let combined = format!("{stderr}\n{stdout}");
    let lines: Vec<&str> = combined
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .collect();

    let error_lines: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|line| line.contains("error"))
        .collect();
    let selected: Vec<&str> = if error_lines.is_empty() {
        lines
            .iter()
            .rev()
            .take(MAX_DETAIL_LINES)
            .rev()
            .copied()
            .collect()
    } else {
        error_lines.into_iter().take(MAX_DETAIL_LINES).collect()
    };

    let mut details = selected.join("\n");
    if details.len() > MAX_DETAIL_CHARS {
        let mut cut = MAX_DETAIL_CHARS;
        while !details.is_char_boundary(cut) {
            cut -= 1;
        }
        details.truncate(cut);
        details.push('…');
    }
    details
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "medusa-verify-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id(),
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn detects_cargo_for_rust_changes_only() {
        let dir = temp_dir("cargo");
        fs::write(dir.join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();

        let verifier = detect_verifier(&dir, &["src/main.rs".to_string()]).unwrap();
        assert_eq!(verifier.label, "cargo check");

        // Doc-only changes must not trigger a build.
        assert!(detect_verifier(&dir, &["README.md".to_string()]).is_none());
        assert!(detect_verifier(&dir, &[]).is_none());
    }

    #[test]
    fn detects_python_compile_for_py_changes_without_manifest() {
        let dir = temp_dir("py");
        let verifier = detect_verifier(&dir, &["tool.py".to_string()]).unwrap();
        assert_eq!(verifier.label, "python py_compile");
        assert!(verifier.args.iter().any(|arg| arg == "tool.py"));
    }

    #[test]
    fn python_verification_passes_and_fails_end_to_end() {
        let dir = temp_dir("py-e2e");
        fs::write(dir.join("good.py"), "x = 1\n").unwrap();
        fs::write(dir.join("bad.py"), "def broken(:\n").unwrap();
        let cancel = CancelToken::default();

        let ok = verify_after_mutation(&dir, &["good.py".to_string()], &cancel).unwrap();
        assert!(ok.starts_with("verify: python py_compile ok"), "{ok}");

        let failed = verify_after_mutation(&dir, &["bad.py".to_string()], &cancel).unwrap();
        assert!(failed.contains("FAILED"), "{failed}");
        assert!(failed.to_lowercase().contains("error"), "{failed}");
    }

    #[test]
    fn cancelled_turns_skip_verification_entirely() {
        let dir = temp_dir("py-cancel");
        fs::write(dir.join("bad.py"), "def broken(:\n").unwrap();
        let cancel = CancelToken::new();
        cancel.cancel();

        assert!(verify_after_mutation(&dir, &["bad.py".to_string()], &cancel).is_none());
    }

    #[test]
    fn failure_details_prefer_error_lines_and_cap_length() {
        let stderr = "warning: unused import\nsrc/a.rs:3:1: error[E0308]: mismatched types\nnote: expected i32\n";
        let details = failure_details("", stderr);
        assert!(details.contains("error[E0308]"));
        assert!(!details.contains("warning: unused import"));

        let flood = "error: x\n".repeat(5_000);
        assert!(failure_details("", &flood).len() <= MAX_DETAIL_CHARS + 4);
    }
}
