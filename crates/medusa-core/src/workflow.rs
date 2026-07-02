use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

use color_eyre::eyre::{Result, WrapErr};
use serde::{Deserialize, Serialize};

use crate::{
    model::{ConversationMessage, DirectCodexBackend, ModelStreamEvent},
    tools::ToolRuntime,
};

const DEFAULT_MAX_AGENTS: usize = 12;

#[derive(Debug, Clone)]
pub struct WorkflowRuntime {
    workspace: PathBuf,
    max_agents: usize,
    memory_context: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowPlan {
    pub run_id: String,
    pub title: String,
    pub task: String,
    pub phases: Vec<WorkflowPhasePlan>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowPhasePlan {
    pub name: String,
    pub objective: String,
    pub agents: Vec<SubagentSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentSpec {
    pub name: String,
    pub role: String,
    pub prompt: String,
    pub allow_mutation: bool,
    #[serde(default)]
    pub tool_policy: SubagentToolPolicy,
}

impl SubagentSpec {
    fn mutation_allowed(&self) -> bool {
        self.allow_mutation || self.tool_policy.allows_mutation()
    }
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

    fn label(self) -> &'static str {
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
                "Use fs_list, file_search, and file_read for inspection. Do not edit files or apply patches."
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkflowEvent {
    RunStarted {
        run_id: String,
        title: String,
        task: String,
        phases: Vec<WorkflowPhasePlan>,
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
    RunFinished {
        run_id: String,
        status: WorkflowStatus,
        summary: String,
    },
}

impl WorkflowRuntime {
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        let max_agents = std::env::var("MEDUSA_WORKFLOW_MAX_AGENTS")
            .ok()
            .and_then(|value| value.parse().ok())
            .filter(|value| *value > 0)
            .unwrap_or(DEFAULT_MAX_AGENTS);

        Self {
            workspace: workspace.into(),
            max_agents,
            memory_context: None,
        }
    }

    pub fn with_memory_context(mut self, memory_context: impl Into<String>) -> Self {
        let memory_context = memory_context.into();
        if !memory_context.trim().is_empty() {
            self.memory_context = Some(memory_context);
        }
        self
    }

    pub fn plan_for_task(&self, task: impl Into<String>) -> WorkflowPlan {
        let task = task.into();
        let run_id = workflow_run_id();
        let title = workflow_title(&task);
        let mut phases = vec![
            WorkflowPhasePlan {
                name: "recon".to_string(),
                objective: "Map the relevant project surface and identify a focused approach."
                    .to_string(),
                agents: vec![
                    SubagentSpec {
                        name: "mapper".to_string(),
                        role: "codebase mapper".to_string(),
                        prompt: "Inspect the workspace for files, modules, and tests relevant to the task. Do not edit files. Return concise findings and the likely implementation path.".to_string(),
                        allow_mutation: false,
                        tool_policy: SubagentToolPolicy::ShellRead,
                    },
                    SubagentSpec {
                        name: "risk-reviewer".to_string(),
                        role: "risk reviewer".to_string(),
                        prompt: "Independently review the task for likely edge cases, hidden constraints, and verification needs. Do not edit files. Return risks and checks.".to_string(),
                        allow_mutation: false,
                        tool_policy: SubagentToolPolicy::ShellRead,
                    },
                ],
            },
            WorkflowPhasePlan {
                name: "synthesis".to_string(),
                objective: "Merge recon outputs into one implementation plan and verification strategy."
                    .to_string(),
                agents: vec![SubagentSpec {
                    name: "synthesizer".to_string(),
                    role: "workflow synthesis agent".to_string(),
                    prompt: "Combine prior subagent findings into a tight implementation plan. Resolve conflicts, name constraints, and identify exact verification steps. Do not edit files.".to_string(),
                    allow_mutation: false,
                    tool_policy: SubagentToolPolicy::ReadOnly,
                }],
            },
            WorkflowPhasePlan {
                name: "implementation".to_string(),
                objective: "Make the smallest correct change using the recon findings.".to_string(),
                agents: vec![SubagentSpec {
                    name: "implementer".to_string(),
                    role: "implementation agent".to_string(),
                    prompt: "Implement the requested change. Use file_edit for exact one-file changes, file_patch for structural edits, and terminal_exec for focused checks. Keep scope tight.".to_string(),
                    allow_mutation: true,
                    tool_policy: SubagentToolPolicy::Edit,
                }],
            },
            WorkflowPhasePlan {
                name: "verification".to_string(),
                objective: "Verify the result and produce the final report.".to_string(),
                agents: vec![SubagentSpec {
                    name: "verifier".to_string(),
                    role: "verification agent".to_string(),
                    prompt: "Inspect the final workspace state, run focused verification where appropriate, and return a user-facing summary with pass/fail status and any residual risks.".to_string(),
                    allow_mutation: false,
                    tool_policy: SubagentToolPolicy::Verify,
                }],
            },
        ];

        let total_agents = phases.iter().map(|phase| phase.agents.len()).sum::<usize>();
        if total_agents > self.max_agents {
            let mut remaining = self.max_agents;
            phases.retain_mut(|phase| {
                if remaining == 0 {
                    return false;
                }
                phase.agents.truncate(remaining);
                remaining = remaining.saturating_sub(phase.agents.len());
                true
            });
        }

        WorkflowPlan {
            run_id,
            title,
            task,
            phases,
        }
    }

    pub fn run_task<F>(
        &self,
        task: impl Into<String>,
        backend: DirectCodexBackend,
        tools: ToolRuntime,
        mut emit: F,
    ) -> Result<WorkflowRunReport>
    where
        F: FnMut(WorkflowEvent) -> Result<()>,
    {
        let plan = self.plan_for_task(task);
        self.run_plan(plan, backend, tools, &mut emit)
    }

    pub fn run_plan<F>(
        &self,
        plan: WorkflowPlan,
        backend: DirectCodexBackend,
        tools: ToolRuntime,
        emit: &mut F,
    ) -> Result<WorkflowRunReport>
    where
        F: FnMut(WorkflowEvent) -> Result<()>,
    {
        emit(WorkflowEvent::RunStarted {
            run_id: plan.run_id.clone(),
            title: plan.title.clone(),
            task: plan.task.clone(),
            phases: plan.phases.clone(),
        })?;

        let mut prior_results = Vec::new();
        let mut phase_reports = Vec::new();

        for (phase_index, phase) in plan.phases.iter().enumerate() {
            emit(WorkflowEvent::PhaseStarted {
                run_id: plan.run_id.clone(),
                phase_index,
                name: phase.name.clone(),
                agent_count: phase.agents.len(),
            })?;

            let (agent_reports, phase_status) = self.run_phase_agents(
                &plan,
                phase_index,
                phase,
                &prior_results,
                backend.clone(),
                tools.clone(),
                emit,
            )?;

            for report in &agent_reports {
                prior_results.push(format!(
                    "[{}:{}:{}]\n{}",
                    phase.name,
                    report.name,
                    status_label(report.status),
                    report.output
                ));
            }

            emit(WorkflowEvent::PhaseFinished {
                run_id: plan.run_id.clone(),
                phase_index,
                name: phase.name.clone(),
                status: phase_status,
            })?;

            phase_reports.push(WorkflowPhaseReport {
                name: phase.name.clone(),
                agents: agent_reports,
            });

            if phase_status == WorkflowStatus::Failed {
                break;
            }
        }

        let run_status = workflow_status_from_phase_reports(&phase_reports);
        let summary = summarize_workflow_report(&phase_reports, run_status);
        emit(WorkflowEvent::RunFinished {
            run_id: plan.run_id.clone(),
            status: run_status,
            summary: summary.clone(),
        })?;

        Ok(WorkflowRunReport {
            run_id: plan.run_id,
            title: plan.title,
            task: plan.task,
            phases: phase_reports,
            summary,
            status: run_status,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn run_phase_agents<F>(
        &self,
        plan: &WorkflowPlan,
        phase_index: usize,
        phase: &WorkflowPhasePlan,
        prior_results: &[String],
        backend: DirectCodexBackend,
        tools: ToolRuntime,
        emit: &mut F,
    ) -> Result<(Vec<SubagentReport>, WorkflowStatus)>
    where
        F: FnMut(WorkflowEvent) -> Result<()>,
    {
        if phase_allows_parallel(phase) {
            self.run_phase_agents_parallel(
                plan,
                phase_index,
                phase,
                prior_results,
                backend,
                tools,
                emit,
            )
        } else {
            self.run_phase_agents_sequential(
                plan,
                phase_index,
                phase,
                prior_results,
                backend,
                tools,
                emit,
            )
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn run_phase_agents_sequential<F>(
        &self,
        plan: &WorkflowPlan,
        phase_index: usize,
        phase: &WorkflowPhasePlan,
        prior_results: &[String],
        backend: DirectCodexBackend,
        tools: ToolRuntime,
        emit: &mut F,
    ) -> Result<(Vec<SubagentReport>, WorkflowStatus)>
    where
        F: FnMut(WorkflowEvent) -> Result<()>,
    {
        let mut reports = Vec::new();
        let mut phase_status = WorkflowStatus::Succeeded;

        for (agent_index, agent) in phase.agents.iter().enumerate() {
            emit_agent_started(plan, phase_index, agent_index, agent, emit)?;
            let report = self
                .run_subagent(
                    plan,
                    phase,
                    agent,
                    prior_results,
                    backend.clone(),
                    tools.clone(),
                )
                .unwrap_or_else(|error| subagent_failure_report(agent, error.to_string()));
            phase_status = phase_status.combine(report.status);
            emit_agent_finished(plan, phase_index, agent_index, &report, emit)?;
            reports.push(report);

            if phase_status == WorkflowStatus::Failed {
                break;
            }
        }

        Ok((reports, phase_status))
    }

    #[allow(clippy::too_many_arguments)]
    fn run_phase_agents_parallel<F>(
        &self,
        plan: &WorkflowPlan,
        phase_index: usize,
        phase: &WorkflowPhasePlan,
        prior_results: &[String],
        backend: DirectCodexBackend,
        tools: ToolRuntime,
        emit: &mut F,
    ) -> Result<(Vec<SubagentReport>, WorkflowStatus)>
    where
        F: FnMut(WorkflowEvent) -> Result<()>,
    {
        let (sender, receiver) = mpsc::channel();
        let mut handles = Vec::new();

        for (agent_index, agent) in phase.agents.iter().cloned().enumerate() {
            emit_agent_started(plan, phase_index, agent_index, &agent, emit)?;

            let sender = sender.clone();
            let runtime = self.clone();
            let plan = plan.clone();
            let phase = phase.clone();
            let prior_results = prior_results.to_vec();
            let backend = backend.clone();
            let tools = tools.clone();
            handles.push(thread::spawn(move || {
                let report = runtime
                    .run_subagent(&plan, &phase, &agent, &prior_results, backend, tools)
                    .unwrap_or_else(|error| subagent_failure_report(&agent, error.to_string()));
                let _ = sender.send((agent_index, report));
            }));
        }
        drop(sender);

        let mut reports = vec![None; phase.agents.len()];
        while let Ok((agent_index, report)) = receiver.recv() {
            emit_agent_finished(plan, phase_index, agent_index, &report, emit)?;
            if let Some(slot) = reports.get_mut(agent_index) {
                *slot = Some(report);
            }
        }

        for (agent_index, handle) in handles.into_iter().enumerate() {
            if handle.join().is_err() && reports.get(agent_index).is_some_and(Option::is_none) {
                let report =
                    subagent_failure_report(&phase.agents[agent_index], "subagent panicked");
                emit_agent_finished(plan, phase_index, agent_index, &report, emit)?;
                reports[agent_index] = Some(report);
            }
        }

        let reports = reports
            .into_iter()
            .enumerate()
            .map(|(agent_index, report)| {
                report.unwrap_or_else(|| {
                    subagent_failure_report(
                        &phase.agents[agent_index],
                        "subagent ended without a report",
                    )
                })
            })
            .collect::<Vec<_>>();
        let phase_status = phase_status_from_reports(&reports);

        Ok((reports, phase_status))
    }

    fn run_subagent(
        &self,
        plan: &WorkflowPlan,
        phase: &WorkflowPhasePlan,
        agent: &SubagentSpec,
        prior_results: &[String],
        backend: DirectCodexBackend,
        tools: ToolRuntime,
    ) -> Result<SubagentReport> {
        let prompt = subagent_prompt(
            &self.workspace,
            self.memory_context.as_deref(),
            plan,
            phase,
            agent,
            prior_results,
        );
        let mut output = String::new();
        let mut tool_counts = BTreeMap::new();
        let mut saw_failed_tool = false;

        let messages = [ConversationMessage {
            role: "user".to_string(),
            content: prompt,
            attachments: Vec::new(),
        }];
        let handle_event = |event| {
            match event {
                ModelStreamEvent::Delta(delta) => output.push_str(&delta),
                ModelStreamEvent::ToolStart { name, .. } => {
                    *tool_counts.entry(name).or_insert(0) += 1;
                }
                ModelStreamEvent::ToolResult { output, .. } => {
                    if tool_result_failed(&output) {
                        saw_failed_tool = true;
                    }
                }
                ModelStreamEvent::ReasoningDelta(_)
                | ModelStreamEvent::Done { .. }
                | ModelStreamEvent::Error(_) => {}
            }
            Ok(())
        };

        if agent.mutation_allowed() {
            backend.chat_stream_messages(&messages, tools, handle_event)
        } else {
            backend.chat_stream_messages_read_only(&messages, tools, handle_event)
        }
        .wrap_err_with(|| format!("{} subagent failed", agent.name))?;

        let output = output.trim().to_string();
        Ok(SubagentReport {
            name: agent.name.clone(),
            role: agent.role.clone(),
            status: if saw_failed_tool {
                WorkflowStatus::Failed
            } else {
                WorkflowStatus::Succeeded
            },
            output: if output.is_empty() {
                "completed without text output".to_string()
            } else {
                output
            },
            tool_counts,
        })
    }
}

fn phase_allows_parallel(phase: &WorkflowPhasePlan) -> bool {
    phase.agents.len() > 1 && phase.agents.iter().all(|agent| !agent.mutation_allowed())
}

fn emit_agent_started<F>(
    plan: &WorkflowPlan,
    phase_index: usize,
    agent_index: usize,
    agent: &SubagentSpec,
    emit: &mut F,
) -> Result<()>
where
    F: FnMut(WorkflowEvent) -> Result<()>,
{
    emit(WorkflowEvent::AgentStarted {
        run_id: plan.run_id.clone(),
        phase_index,
        agent_index,
        name: agent.name.clone(),
        role: agent.role.clone(),
    })
}

fn emit_agent_finished<F>(
    plan: &WorkflowPlan,
    phase_index: usize,
    agent_index: usize,
    report: &SubagentReport,
    emit: &mut F,
) -> Result<()>
where
    F: FnMut(WorkflowEvent) -> Result<()>,
{
    emit(WorkflowEvent::AgentFinished {
        run_id: plan.run_id.clone(),
        phase_index,
        agent_index,
        name: report.name.clone(),
        status: report.status,
        output: report.output.clone(),
        tool_counts: report.tool_counts.clone(),
    })
}

fn subagent_failure_report(agent: &SubagentSpec, error: impl Into<String>) -> SubagentReport {
    SubagentReport {
        name: agent.name.clone(),
        role: agent.role.clone(),
        status: WorkflowStatus::Failed,
        output: format!("subagent failed: {}", error.into()),
        tool_counts: BTreeMap::new(),
    }
}

fn subagent_prompt(
    workspace: &Path,
    memory_context: Option<&str>,
    plan: &WorkflowPlan,
    phase: &WorkflowPhasePlan,
    agent: &SubagentSpec,
    prior_results: &[String],
) -> String {
    let mut prompt = format!(
        "You are a Medusa workflow subagent.\n\
Parent workflow: {}\n\
Workspace: {}\n\
User task: {}\n\
Current phase: {} — {}\n\
Your agent name: {}\n\
Your role: {}\n\
Tool policy: {}\n\
Your directive: {}\n\n\
Rules:\n\
- Stay inside your role and scope.\n\
- Use Medusa file tools for listing, searching, and reading.\n\
- Return compact, structured findings.\n",
        plan.title,
        workspace.display(),
        plan.task,
        phase.name,
        phase.objective,
        agent.name,
        agent.role,
        agent.tool_policy.label(),
        agent.prompt
    );

    prompt.push_str("- ");
    prompt.push_str(agent.tool_policy.instructions());
    prompt.push('\n');

    if agent.mutation_allowed() {
        prompt.push_str("- After editing, run focused verification when practical.\n");
    } else {
        prompt.push_str("- Do not apply patches. Do not run broad destructive commands.\n");
    }

    if let Some(memory_context) = memory_context {
        prompt.push_str("\nParent session memory:\n");
        prompt.push_str(memory_context.trim());
        prompt.push('\n');
    }

    if !prior_results.is_empty() {
        prompt.push_str("\nPrior workflow results:\n");
        for result in prior_results.iter().rev().take(6).rev() {
            prompt.push_str(result);
            prompt.push_str("\n\n");
        }
    }

    prompt.push_str(
        "\nReturn exactly this concise structure:\n\
## Findings\n\
- ...\n\
## Actions\n\
- ...\n\
## Verification\n\
- ...\n\
## Handoff\n\
- ...",
    );
    prompt
}

fn workflow_title(task: &str) -> String {
    let trimmed = task.trim();
    if trimmed.is_empty() {
        return "workflow".to_string();
    }
    let title = trimmed
        .split_whitespace()
        .take(8)
        .collect::<Vec<_>>()
        .join(" ");
    if title.len() > 64 {
        format!("{}...", &title[..61])
    } else {
        title
    }
}

fn workflow_run_id() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis());
    format!("workflow-{millis}")
}

fn status_label(status: WorkflowStatus) -> &'static str {
    match status {
        WorkflowStatus::Running => "running",
        WorkflowStatus::Succeeded => "succeeded",
        WorkflowStatus::PartiallySucceeded => "partial",
        WorkflowStatus::Failed => "failed",
    }
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

fn workflow_status_from_phase_reports(phases: &[WorkflowPhaseReport]) -> WorkflowStatus {
    let mut statuses = phases
        .iter()
        .map(|phase| phase_status_from_reports(&phase.agents));
    let Some(first) = statuses.next() else {
        return WorkflowStatus::Failed;
    };
    statuses.fold(first, WorkflowStatus::combine)
}

fn summarize_workflow_report(phases: &[WorkflowPhaseReport], status: WorkflowStatus) -> String {
    let agents = phases.iter().map(|phase| phase.agents.len()).sum::<usize>();
    let succeeded = phases
        .iter()
        .flat_map(|phase| &phase.agents)
        .filter(|agent| agent.status == WorkflowStatus::Succeeded)
        .count();
    let failed_agents = phases
        .iter()
        .flat_map(|phase| {
            phase.agents.iter().filter_map(|agent| {
                (agent.status == WorkflowStatus::Failed).then_some((phase.name.as_str(), agent))
            })
        })
        .collect::<Vec<_>>();
    let failed = failed_agents.len();
    let last_success = phases
        .iter()
        .rev()
        .flat_map(|phase| phase.agents.iter().rev())
        .find(|agent| agent.status == WorkflowStatus::Succeeded && !agent.output.trim().is_empty())
        .map(|agent| agent.output.trim());
    let last_output = phases
        .iter()
        .rev()
        .flat_map(|phase| phase.agents.iter().rev())
        .find(|agent| !agent.output.trim().is_empty())
        .map(|agent| agent.output.trim())
        .unwrap_or("workflow completed");

    let headline = match status {
        WorkflowStatus::Succeeded => format!(
            "workflow completed: {agents} agents across {} phases",
            phases.len()
        ),
        WorkflowStatus::Running => "workflow still running".to_string(),
        WorkflowStatus::PartiallySucceeded => {
            format!(
                "workflow partially completed: {succeeded}/{agents} agents succeeded, {failed} failed"
            )
        }
        WorkflowStatus::Failed => format!("workflow failed: {failed}/{agents} agents failed"),
    };

    let mut sections = vec![headline];
    if !failed_agents.is_empty() {
        let failures = failed_agents
            .iter()
            .take(4)
            .map(|(phase, agent)| {
                format!(
                    "- {phase}/{}: {}",
                    agent.name,
                    compact(agent.output.trim(), 260)
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        sections.push(format!("Failed agents\n{failures}"));
    }

    if status == WorkflowStatus::PartiallySucceeded {
        if let Some(last_success) = last_success {
            sections.push(format!(
                "Useful completed work\n{}",
                compact(last_success, 1_200)
            ));
        }
    } else {
        sections.push(compact(last_output, 1_600));
    }

    sections.join("\n\n")
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
    fn workflow_plan_has_bounded_phases() {
        let runtime = WorkflowRuntime::new(PathBuf::from("/tmp/project"));
        let plan = runtime.plan_for_task("audit the repo and fix tests");

        assert_eq!(plan.phases.len(), 4);
        assert!(plan.phases.iter().any(|phase| phase.name == "recon"));
        assert!(plan.phases.iter().any(|phase| phase.name == "synthesis"));
        assert!(
            plan.phases
                .iter()
                .flat_map(|phase| &phase.agents)
                .any(|agent| {
                    agent.name == "implementer"
                        && agent.allow_mutation
                        && agent.tool_policy == SubagentToolPolicy::Edit
                })
        );
    }

    #[test]
    fn subagent_prompt_includes_memory_policy_and_output_contract() {
        let runtime = WorkflowRuntime::new(PathBuf::from("/workspace"))
            .with_memory_context("semantic memory:\n- preference: keep answers concise");
        let plan = runtime.plan_for_task("implement code changes");
        let phase = &plan.phases[0];
        let agent = &phase.agents[0];

        let prompt = subagent_prompt(
            &runtime.workspace,
            runtime.memory_context.as_deref(),
            &plan,
            phase,
            agent,
            &[],
        );

        assert!(prompt.contains("Do not edit files"));
        assert!(prompt.contains("Tool policy: shell-read"));
        assert!(prompt.contains("preference: keep answers concise"));
        assert!(prompt.contains("## Findings"));
        assert!(prompt.contains("## Handoff"));
        assert!(prompt.contains("implement code changes"));
        assert!(!agent.allow_mutation);
    }

    #[test]
    fn read_only_phases_parallelize_but_edit_phases_do_not() {
        let runtime = WorkflowRuntime::new(PathBuf::from("/tmp/project"));
        let plan = runtime.plan_for_task("audit the repo and fix tests");

        assert!(phase_allows_parallel(&plan.phases[0]));
        assert!(!phase_allows_parallel(
            plan.phases
                .iter()
                .find(|phase| phase.name == "implementation")
                .unwrap()
        ));
    }

    #[test]
    fn workflow_status_distinguishes_partial_success_from_total_failure() {
        let partial = vec![
            WorkflowPhaseReport {
                name: "implementation".to_string(),
                agents: vec![SubagentReport {
                    name: "implementer".to_string(),
                    role: "implementation agent".to_string(),
                    status: WorkflowStatus::Succeeded,
                    output: "Moved terminal helpers into terminal.rs and cargo check passed."
                        .to_string(),
                    tool_counts: BTreeMap::new(),
                }],
            },
            WorkflowPhaseReport {
                name: "verification".to_string(),
                agents: vec![SubagentReport {
                    name: "verifier".to_string(),
                    role: "verification agent".to_string(),
                    status: WorkflowStatus::Failed,
                    output: "subagent failed: backend overloaded".to_string(),
                    tool_counts: BTreeMap::new(),
                }],
            },
        ];

        let status = workflow_status_from_phase_reports(&partial);
        let summary = summarize_workflow_report(&partial, status);

        assert_eq!(status, WorkflowStatus::PartiallySucceeded);
        assert!(summary.contains("workflow partially completed"));
        assert!(summary.contains("verification/verifier"));
        assert!(summary.contains("Moved terminal helpers"));

        let total_failure = vec![WorkflowPhaseReport {
            name: "recon".to_string(),
            agents: vec![SubagentReport {
                name: "mapper".to_string(),
                role: "codebase mapper".to_string(),
                status: WorkflowStatus::Failed,
                output: "subagent failed: unavailable".to_string(),
                tool_counts: BTreeMap::new(),
            }],
        }];

        assert_eq!(
            workflow_status_from_phase_reports(&total_failure),
            WorkflowStatus::Failed
        );
    }
}
