use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::agents::AgentRegistry;

mod journal;
pub mod script;
pub use script::WorkflowScript;

#[derive(Debug, Clone)]
pub struct WorkflowRuntime {
    workspace: PathBuf,
    memory_context: Option<String>,
    agent_registry: AgentRegistry,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentToolPolicy {
    #[default]
    ReadOnly,
    ShellRead,
    Edit,
    Verify,
}

impl SubagentToolPolicy {
    fn allows_mutation(self) -> bool {
        matches!(self, Self::Edit)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::ShellRead => "shell-read",
            Self::Edit => "edit",
            Self::Verify => "verify",
        }
    }

    fn instructions(self) -> &'static str {
        match self {
            Self::ReadOnly => {
                "Use fs_list, file_search, file_glob, and file_read for inspection. Do not edit files or apply patches."
            }
            Self::ShellRead => {
                "Prefer file tools for inspection. You may use terminal_exec for safe read-only commands such as rg, sed, git status, cargo test, or format/check commands. Do not edit files."
            }
            Self::Edit => {
                "You may edit files when it directly advances the task. Prefer file_edit for exact one-file changes and file_patch for structural changes."
            }
            Self::Verify => {
                "Use inspection and focused verification commands. Do not edit files. Report pass/fail status and residual risk."
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowRunReport {
    pub run_id: String,
    pub title: String,
    pub task: String,
    pub phases: Vec<WorkflowPhaseReport>,
    pub summary: String,
    pub status: WorkflowStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowPhaseReport {
    pub name: String,
    pub agents: Vec<SubagentReport>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentReport {
    pub name: String,
    pub role: String,
    pub status: WorkflowStatus,
    pub output: String,
    pub tool_counts: BTreeMap<String, usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkflowStatus {
    Running,
    Succeeded,
    PartiallySucceeded,
    Failed,
}

impl WorkflowStatus {
    fn combine(self, other: Self) -> Self {
        match (self, other) {
            (Self::Running, _) | (_, Self::Running) => Self::Running,
            (Self::Succeeded, Self::Succeeded) => Self::Succeeded,
            (Self::Failed, Self::Failed) => Self::Failed,
            (Self::PartiallySucceeded, _) | (_, Self::PartiallySucceeded) => {
                Self::PartiallySucceeded
            }
            (Self::Succeeded, Self::Failed) | (Self::Failed, Self::Succeeded) => {
                Self::PartiallySucceeded
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkflowEvent {
    RunStarted {
        run_id: String,
        title: String,
        task: String,
    },
    PhaseStarted {
        run_id: String,
        phase_index: usize,
        name: String,
        agent_count: usize,
    },
    AgentStarted {
        run_id: String,
        phase_index: usize,
        agent_index: usize,
        name: String,
        role: String,
        tool_policy: SubagentToolPolicy,
    },
    AgentFinished {
        run_id: String,
        phase_index: usize,
        agent_index: usize,
        name: String,
        status: WorkflowStatus,
        output: String,
        tool_counts: BTreeMap<String, usize>,
    },
    PhaseFinished {
        run_id: String,
        phase_index: usize,
        name: String,
        status: WorkflowStatus,
    },
    Log {
        run_id: String,
        message: String,
    },
    RunFinished {
        run_id: String,
        status: WorkflowStatus,
        summary: String,
    },
}

impl WorkflowEvent {
    pub fn run_id(&self) -> &str {
        match self {
            Self::RunStarted { run_id, .. }
            | Self::PhaseStarted { run_id, .. }
            | Self::AgentStarted { run_id, .. }
            | Self::AgentFinished { run_id, .. }
            | Self::PhaseFinished { run_id, .. }
            | Self::Log { run_id, .. }
            | Self::RunFinished { run_id, .. } => run_id,
        }
    }
}

impl WorkflowRuntime {
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        let workspace = workspace.into();
        let agent_registry = AgentRegistry::load(&workspace).unwrap_or_default();
        Self {
            workspace,
            memory_context: None,
            agent_registry,
        }
    }

    pub fn with_memory_context(mut self, memory_context: impl Into<String>) -> Self {
        let memory_context = memory_context.into();
        if !memory_context.trim().is_empty() {
            self.memory_context = Some(memory_context);
        }
        self
    }

    /// Replace the named-agent registry resolved for `agentType` specs.
    pub fn with_agent_registry(mut self, agent_registry: AgentRegistry) -> Self {
        self.agent_registry = agent_registry;
        self
    }
}

fn workflow_run_id() -> String {
    static WORKFLOW_SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis());
    let sequence = WORKFLOW_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("workflow-{millis}-{}-{sequence}", std::process::id())
}

fn tool_result_failed(output: &str) -> bool {
    let normalized = output.trim_start().to_ascii_lowercase();
    normalized.starts_with("error:")
        || normalized.contains("\nerror:")
        || normalized.contains("exit: 1\n")
        || normalized.contains("exit: 101\n")
}

fn phase_status_from_reports(reports: &[SubagentReport]) -> WorkflowStatus {
    let mut statuses = reports.iter().map(|report| report.status);
    let Some(first) = statuses.next() else {
        return WorkflowStatus::Failed;
    };
    statuses.fold(first, WorkflowStatus::combine)
}

fn compact(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let compacted = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{compacted}...")
    } else {
        compacted
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workflow_status_combines_mixed_agent_results_as_partial() {
        let reports = vec![
            SubagentReport {
                name: "scanner".to_string(),
                role: "scan".to_string(),
                status: WorkflowStatus::Succeeded,
                output: "found one issue".to_string(),
                tool_counts: BTreeMap::new(),
            },
            SubagentReport {
                name: "verifier".to_string(),
                role: "verify".to_string(),
                status: WorkflowStatus::Failed,
                output: "backend unavailable".to_string(),
                tool_counts: BTreeMap::new(),
            },
        ];

        assert_eq!(
            phase_status_from_reports(&reports),
            WorkflowStatus::PartiallySucceeded
        );
        assert_eq!(phase_status_from_reports(&[]), WorkflowStatus::Failed);
    }

    #[test]
    fn tool_policies_keep_mutation_explicit() {
        assert!(!SubagentToolPolicy::ReadOnly.allows_mutation());
        assert!(!SubagentToolPolicy::ShellRead.allows_mutation());
        assert!(SubagentToolPolicy::Edit.allows_mutation());
        assert!(!SubagentToolPolicy::Verify.allows_mutation());
    }
}
