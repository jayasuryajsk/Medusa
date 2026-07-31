use std::collections::{HashMap, hash_map::DefaultHasher};
use std::hash::{Hash, Hasher};

use crate::harness::HarnessPolicy;

const MAX_EVIDENCE_ENTRIES: usize = 16;
const MAX_CHANGED_FILES: usize = 32;
const DEFAULT_NO_PROGRESS_REPETITIONS: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VerificationState {
    Unknown,
    Passed,
    Failed,
}

#[derive(Debug, Clone)]
pub(crate) struct TurnOrchestrator {
    exploration_required: bool,
    workflow_required: bool,
    workflow_completed: bool,
    workflow_retry_sent: bool,
    verification: VerificationState,
    verification_retry_sent: bool,
    observations: Vec<String>,
    changed_files: Vec<String>,
    changed_file_overflow: usize,
}

impl TurnOrchestrator {
    pub(crate) fn new(policy: HarnessPolicy) -> Self {
        Self {
            exploration_required: policy.requires_initial_exploration(),
            workflow_required: policy.requires_workflow(),
            workflow_completed: false,
            workflow_retry_sent: false,
            verification: VerificationState::Unknown,
            verification_retry_sent: false,
            observations: Vec::new(),
            changed_files: Vec::new(),
            changed_file_overflow: 0,
        }
    }

    pub(crate) fn native_mutation_allowed(&self) -> bool {
        let exploration_satisfied = !self.exploration_required || !self.observations.is_empty();
        let workflow_satisfied = !self.workflow_required || self.workflow_completed;
        exploration_satisfied && workflow_satisfied
    }

    pub(crate) fn record_execution(
        &mut self,
        name: &str,
        summary: &str,
        output: &str,
        failed: bool,
        changed_files: &[String],
    ) {
        if !failed && is_observation_tool(name) {
            let entry = if summary.trim().is_empty() {
                name.to_string()
            } else {
                format!("{name}: {}", compact(summary, 180))
            };
            self.push_observation(entry);
        }

        if name == "workflow_run" && !failed {
            self.workflow_completed = true;
        }

        if !changed_files.is_empty() {
            self.verification = VerificationState::Unknown;
            for path in changed_files {
                if self.changed_files.contains(path) {
                    continue;
                }
                if self.changed_files.len() < MAX_CHANGED_FILES {
                    self.changed_files.push(path.clone());
                } else {
                    self.changed_file_overflow += 1;
                }
            }
        }

        if let Some(verification) = verification_state(output) {
            self.verification = verification;
        }
    }

    pub(crate) fn completion_feedback(&mut self) -> Option<String> {
        if self.workflow_required && !self.workflow_completed && !self.workflow_retry_sent {
            self.workflow_retry_sent = true;
            return Some(
                "Medusa orchestration gate: this turn was routed to workflow execution, but no \
successful workflow_run has completed. Run the task-specific workflow now, or explicitly explain \
why it cannot run and which completion criterion remains blocked."
                    .to_string(),
            );
        }

        if self.verification == VerificationState::Failed && !self.verification_retry_sent {
            self.verification_retry_sent = true;
            return Some(
                "Medusa convergence gate: the latest post-edit verification failed. Do not claim \
completion yet. Inspect the failure, repair it, and validate again; if it cannot be repaired within \
the user's authorization, explicitly report the blocker and unfinished criterion."
                    .to_string(),
            );
        }

        None
    }

    pub(crate) fn context(&self) -> Option<String> {
        if self.observations.is_empty()
            && self.changed_files.is_empty()
            && self.verification == VerificationState::Unknown
        {
            return None;
        }

        let mut lines = vec!["Medusa evidence ledger for this turn:".to_string()];
        if !self.observations.is_empty() {
            lines.push(format!("- observed: {}", self.observations.join(" | ")));
        }
        if !self.changed_files.is_empty() {
            let overflow = if self.changed_file_overflow == 0 {
                String::new()
            } else {
                format!(" (+{} more)", self.changed_file_overflow)
            };
            lines.push(format!(
                "- changed: {}{overflow}",
                self.changed_files.join(", ")
            ));
        }
        match self.verification {
            VerificationState::Unknown => {}
            VerificationState::Passed => lines.push("- post-edit verification: passed".to_string()),
            VerificationState::Failed => lines.push("- post-edit verification: failed".to_string()),
        }
        Some(lines.join("\n"))
    }

    fn push_observation(&mut self, entry: String) {
        if self.observations.contains(&entry) {
            return;
        }
        if self.observations.len() == MAX_EVIDENCE_ENTRIES {
            self.observations.remove(0);
        }
        self.observations.push(entry);
    }
}

fn is_observation_tool(name: &str) -> bool {
    matches!(
        name,
        "file_read"
            | "file_search"
            | "file_glob"
            | "fs_list"
            | "explore_batch"
            | "terminal_exec"
            | "web_fetch"
            | "web_search"
            | "workflow_run"
    ) || name.starts_with("mcp_")
}

fn verification_state(output: &str) -> Option<VerificationState> {
    let line = output.lines().find(|line| line.starts_with("verify: "))?;
    let normalized = line.to_ascii_lowercase();
    if normalized.contains(" failed")
        || normalized.contains(" timed out")
        || normalized.contains(" cancelled")
    {
        Some(VerificationState::Failed)
    } else if normalized.contains(" ok") {
        Some(VerificationState::Passed)
    } else {
        None
    }
}

fn compact(value: &str, max_chars: usize) -> String {
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= max_chars {
        return normalized;
    }
    let mut compacted = normalized
        .chars()
        .take(max_chars.saturating_sub(3))
        .collect::<String>();
    compacted.push_str("...");
    compacted
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ToolAttempt<'a> {
    pub(crate) name: &'a str,
    pub(crate) arguments: &'a str,
    pub(crate) output: &'a str,
    pub(crate) failed: bool,
    pub(crate) made_durable_progress: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProgressSignal {
    Progress,
    Warning(String),
    Stalled(String),
}

#[derive(Debug)]
pub(crate) struct NoProgressTracker {
    repetition_limit: usize,
    last_batch: Option<u64>,
    repeated_batch: usize,
    repeated_failures: HashMap<u64, usize>,
}

impl Default for NoProgressTracker {
    fn default() -> Self {
        Self::new(no_progress_repetition_limit())
    }
}

impl NoProgressTracker {
    fn new(repetition_limit: usize) -> Self {
        Self {
            repetition_limit: repetition_limit.max(2),
            last_batch: None,
            repeated_batch: 0,
            repeated_failures: HashMap::new(),
        }
    }

    pub(crate) fn observe(&mut self, attempts: &[ToolAttempt<'_>]) -> ProgressSignal {
        if attempts.is_empty() {
            return ProgressSignal::Progress;
        }

        if attempts.iter().any(|attempt| attempt.made_durable_progress) {
            self.last_batch = None;
            self.repeated_batch = 0;
            self.repeated_failures.clear();
            return ProgressSignal::Progress;
        }

        let batch = fingerprint_batch(attempts);
        if self.last_batch == Some(batch) {
            self.repeated_batch += 1;
        } else {
            self.last_batch = Some(batch);
            self.repeated_batch = 1;
        }

        let mut highest_failure_count = 0;
        for attempt in attempts.iter().filter(|attempt| attempt.failed) {
            let count = self
                .repeated_failures
                .entry(fingerprint_attempt(attempt))
                .and_modify(|count| *count += 1)
                .or_insert(1);
            highest_failure_count = highest_failure_count.max(*count);
        }
        if self.repeated_failures.len() > 64 {
            self.repeated_failures.retain(|_, count| *count > 1);
        }

        let repetitions = self.repeated_batch.max(highest_failure_count);
        if repetitions >= self.repetition_limit {
            return ProgressSignal::Stalled(format!(
                "agent stopped after {repetitions} equivalent tool cycles without durable progress; \
inspect current state and choose a different approach before retrying"
            ));
        }
        if repetitions + 1 == self.repetition_limit {
            return ProgressSignal::Warning(
                "Medusa no-progress warning: the same tool outcome is repeating. Re-read the live \
state, change the hypothesis or method, and do not issue the identical call again."
                    .to_string(),
            );
        }

        ProgressSignal::Progress
    }
}

fn no_progress_repetition_limit() -> usize {
    std::env::var("MEDUSA_NO_PROGRESS_REPETITIONS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .map(|value| value.clamp(2, 10))
        .unwrap_or(DEFAULT_NO_PROGRESS_REPETITIONS)
}

fn fingerprint_batch(attempts: &[ToolAttempt<'_>]) -> u64 {
    let mut hasher = DefaultHasher::new();
    for attempt in attempts {
        fingerprint_attempt(attempt).hash(&mut hasher);
    }
    hasher.finish()
}

fn fingerprint_attempt(attempt: &ToolAttempt<'_>) -> u64 {
    let mut hasher = DefaultHasher::new();
    attempt.name.hash(&mut hasher);
    attempt.arguments.trim().hash(&mut hasher);
    attempt.output.trim().hash(&mut hasher);
    attempt.failed.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attempt<'a>(
        name: &'a str,
        arguments: &'a str,
        output: &'a str,
        failed: bool,
    ) -> ToolAttempt<'a> {
        ToolAttempt {
            name,
            arguments,
            output,
            failed,
            made_durable_progress: false,
        }
    }

    #[test]
    fn repeated_identical_batches_warn_then_stop() {
        let batch = [attempt("file_read", r#"{"paths":["a.rs"]}"#, "same", false)];
        let mut tracker = NoProgressTracker::new(3);

        assert_eq!(tracker.observe(&batch), ProgressSignal::Progress);
        assert!(matches!(
            tracker.observe(&batch),
            ProgressSignal::Warning(_)
        ));
        assert!(matches!(
            tracker.observe(&batch),
            ProgressSignal::Stalled(_)
        ));
    }

    #[test]
    fn durable_progress_resets_repetition_history() {
        let repeated = [attempt("file_patch", "{}", "error", true)];
        let mut tracker = NoProgressTracker::new(3);
        tracker.observe(&repeated);
        assert!(matches!(
            tracker.observe(&repeated),
            ProgressSignal::Warning(_)
        ));

        let progress = [ToolAttempt {
            name: "file_edit",
            arguments: "{}",
            output: "edited files:\na.rs",
            failed: false,
            made_durable_progress: true,
        }];
        assert_eq!(tracker.observe(&progress), ProgressSignal::Progress);
        assert_eq!(tracker.observe(&repeated), ProgressSignal::Progress);
    }

    #[test]
    fn evidence_unlocks_explore_first_mutation() {
        let policy = HarnessPolicy::for_user_prompt("fix the failing tests");
        let mut orchestrator = TurnOrchestrator::new(policy);
        assert!(!orchestrator.native_mutation_allowed());

        orchestrator.record_execution("file_read", "read src/lib.rs", "contents", false, &[]);
        assert!(orchestrator.native_mutation_allowed());
        assert!(
            orchestrator
                .context()
                .expect("evidence context")
                .contains("src/lib.rs")
        );
    }

    #[test]
    fn failed_verification_requests_one_repair_cycle() {
        let policy = HarnessPolicy::for_user_prompt("fix the failing tests");
        let mut orchestrator = TurnOrchestrator::new(policy);
        orchestrator.record_execution(
            "file_edit",
            "edit src/lib.rs",
            "edited files:\nsrc/lib.rs\nverify: cargo check FAILED",
            false,
            &["src/lib.rs".to_string()],
        );

        assert!(
            orchestrator
                .completion_feedback()
                .expect("repair feedback")
                .contains("verification failed")
        );
        assert!(orchestrator.completion_feedback().is_none());
    }
}
