use std::collections::HashMap;

use medusa_core::workflow::{SubagentToolPolicy, WorkflowStatus};
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use crate::animation;
use crate::config::palette;
use crate::constants::{
    CHAT_BOTTOM_PADDING_ROWS, CHAT_IMAGE_PREVIEW_HEIGHT, CHAT_IMAGE_PREVIEW_WIDTH,
    COMPOSER_IMAGE_PREVIEW_HEIGHT, COMPOSER_IMAGE_PREVIEW_WIDTH, IMAGE_PREVIEW_MAX_ZOOM,
    IMAGE_PREVIEW_MIN_ZOOM,
};
use crate::markdown::{inline_markdown_spans, markdown_content_lines};
use crate::styles::*;
use crate::types::*;
use crate::util::{IfEmpty, attachment_label, compact_one_line, tool_summary, truncate};
use medusa_core::session::human_bytes;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct TranscriptCharUsage {
    pub(crate) messages: usize,
    pub(crate) tool_outputs: usize,
    pub(crate) reasoning: usize,
    pub(crate) plans: usize,
}

impl TranscriptCharUsage {
    pub(crate) fn total(&self) -> usize {
        self.messages + self.tool_outputs + self.reasoning + self.plans
    }
}

pub(crate) fn transcript_char_usage(transcript: &[TranscriptItem]) -> TranscriptCharUsage {
    let mut usage = TranscriptCharUsage::default();
    for item in transcript {
        match item {
            TranscriptItem::Message(msg) => usage.messages += msg.content.len(),
            // Tool results are function_call_outputs in model context; they
            // usually dominate usage, so count them too.
            TranscriptItem::Tool(run) => usage.tool_outputs += run.summary.len() + run.detail.len(),
            TranscriptItem::Reasoning(trace) => usage.reasoning += trace.content.len(),
            TranscriptItem::Plan(plan) => {
                usage.plans += plan.summary.len()
                    + plan
                        .items
                        .iter()
                        .map(|item| {
                            item.text.len() + item.evidence.iter().map(String::len).sum::<usize>()
                        })
                        .sum::<usize>();
            }
            TranscriptItem::Decision(decision) => {
                usage.plans += decision.title.len()
                    + decision.reason.len()
                    + decision.answer.as_ref().map_or(0, String::len)
                    + decision
                        .answers
                        .iter()
                        .map(|(key, value)| key.len() + value.len())
                        .sum::<usize>()
                    + decision
                        .questions
                        .iter()
                        .map(|question| {
                            question.prompt.len()
                                + question.options.iter().map(String::len).sum::<usize>()
                        })
                        .sum::<usize>();
            }
            TranscriptItem::Workflow(_) => {}
        }
    }
    usage
}

/// Estimated context composition captured when /context ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ContextReport {
    pub(crate) instructions_tokens: usize,
    pub(crate) system_tokens: usize,
    pub(crate) message_tokens: usize,
    pub(crate) tool_tokens: usize,
    pub(crate) reasoning_tokens: usize,
    pub(crate) plan_tokens: usize,
    pub(crate) budget: usize,
    pub(crate) summary_covers: Option<usize>,
    pub(crate) summary_tokens: usize,
}

impl ContextReport {
    pub(crate) fn total_tokens(&self) -> usize {
        self.instructions_tokens
            + self.system_tokens
            + self.message_tokens
            + self.tool_tokens
            + self.reasoning_tokens
            + self.plan_tokens
    }

    pub(crate) fn percent_used(&self) -> usize {
        self.total_tokens() * 100 / self.budget.max(1)
    }
}

/// Compact token count for footers and toasts: "812 tok", "1.23k tok",
/// "2.05M tok".
pub(crate) fn format_token_count(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.2}M tok", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.2}k tok", tokens as f64 / 1_000.0)
    } else {
        format!("{tokens} tok")
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct PlanProgress {
    pub(crate) pending: usize,
    pub(crate) active: usize,
    pub(crate) done: usize,
    pub(crate) blocked: usize,
}

pub(crate) fn plan_progress(plan: &PlanView) -> PlanProgress {
    let mut progress = PlanProgress::default();
    for item in &plan.items {
        match item.status {
            PlanItemStatus::Pending => progress.pending += 1,
            PlanItemStatus::Active => progress.active += 1,
            PlanItemStatus::Done => progress.done += 1,
            PlanItemStatus::Blocked => progress.blocked += 1,
        }
    }
    progress
}

/// Items shown in the plan strip before folding the tail behind "+N more".
pub(crate) const PLAN_STRIP_MAX_ITEMS: usize = 6;

pub(crate) fn plan_strip_lines(plan: &PlanView) -> Vec<Line<'static>> {
    let progress = plan_progress(plan);
    let mut lines = Vec::new();

    let mut header = vec![
        Span::styled("plan", tool_label_style().add_modifier(Modifier::BOLD)),
        Span::styled(" · ", muted()),
        Span::styled(
            format!("{}/{}", progress.done, plan.items.len()),
            success_style(),
        ),
    ];
    if progress.blocked > 0 {
        header.extend([
            Span::styled(" · ", muted()),
            Span::styled(format!("{} blocked", progress.blocked), error_style()),
        ]);
    }
    if !plan.summary.trim().is_empty() {
        header.extend([
            Span::styled(" · ", muted()),
            Span::styled(truncate(&plan.summary, 72), muted()),
        ]);
    }
    lines.push(Line::from(header));

    // Long plans fold the completed prefix into one line so the strip always
    // centers on what's happening now.
    let mut start = 0;
    if plan.items.len() > PLAN_STRIP_MAX_ITEMS {
        let leading_done = plan
            .items
            .iter()
            .take_while(|item| item.status == PlanItemStatus::Done)
            .count();
        if leading_done > 1 {
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled("✓", success_style()),
                Span::styled(format!(" {leading_done} done"), success_style()),
            ]));
            start = leading_done;
        }
    }

    let remaining = &plan.items[start..];
    let shown = remaining.len().min(PLAN_STRIP_MAX_ITEMS);
    for item in remaining.iter().take(shown) {
        lines.push(Line::from(vec![
            Span::raw("  "),
            plan_status_marker_span(item.status),
            Span::raw(" "),
            Span::styled(truncate(&item.text, 110), plan_status_style(item.status)),
        ]));
    }
    if remaining.len() > shown {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("… +{} more", remaining.len() - shown), muted()),
        ]));
    }

    lines
}

pub(crate) fn plan_status_marker_span(status: PlanItemStatus) -> Span<'static> {
    match status {
        PlanItemStatus::Pending => Span::styled("·", muted()),
        PlanItemStatus::Active => Span::styled("●", prompt_style()),
        PlanItemStatus::Done => Span::styled("✓", success_style()),
        PlanItemStatus::Blocked => Span::styled("×", error_style()),
    }
}

pub(crate) fn plan_status_style(status: PlanItemStatus) -> Style {
    match status {
        PlanItemStatus::Pending => muted(),
        PlanItemStatus::Active => prompt_style(),
        PlanItemStatus::Done => success_style(),
        PlanItemStatus::Blocked => error_style(),
    }
}

pub(crate) fn append_decision_rows(
    rows: &mut Vec<TranscriptRow>,
    decision: &DecisionView,
    selected_question: usize,
) {
    let state = if decision.answered {
        "answered"
    } else {
        "waiting"
    };
    let mut header = vec![
        Span::styled("decision", prompt_style().add_modifier(Modifier::BOLD)),
        Span::styled(" · ", muted()),
        Span::styled(
            format!("{} question(s)", decision.questions.len()),
            value_style(),
        ),
        Span::styled(" · ", muted()),
        Span::styled(
            state,
            if decision.answered {
                success_style()
            } else {
                prompt_style()
            },
        ),
    ];
    if !decision.title.trim().is_empty() {
        header.extend([
            Span::styled(" · ", muted()),
            Span::styled(truncate(&decision.title, 72), muted()),
        ]);
    }
    rows.push(TranscriptRow::text(Line::from(header)));

    let last = decision.questions.len().saturating_sub(1);
    for (index, question) in decision.questions.iter().enumerate() {
        let answered = decision_question_answered(decision, question);
        let selected = !decision.answered && index == selected_question;
        let (marker, marker_style) = if selected {
            ("› ", prompt_style())
        } else if answered {
            ("✓ ", success_style())
        } else {
            ("? ", prompt_style())
        };
        rows.push(TranscriptRow::text(Line::from(vec![
            Span::styled(
                if index == last {
                    "  └─ "
                } else {
                    "  ├─ "
                },
                muted(),
            ),
            Span::styled(marker, marker_style),
            Span::styled(
                truncate(&question.prompt, 120),
                if selected {
                    value_style().add_modifier(Modifier::BOLD)
                } else {
                    value_style()
                },
            ),
        ])));
        let continuation = if index == last { "     " } else { "  │  " };
        if question.kind == DecisionQuestionKind::Choice && !decision.answered {
            for (option_index, option) in question.options.iter().take(4).enumerate() {
                let recommended = question.recommended.as_deref() == Some(option.as_str());
                let picked = decision.answers.get(&question.id) == Some(option);
                let mut spans = vec![
                    Span::styled(continuation, muted()),
                    Span::styled(
                        format!("{} {}. ", if picked { "●" } else { "○" }, option_index + 1),
                        if picked { success_style() } else { muted() },
                    ),
                    Span::styled(
                        truncate(option, 100),
                        if picked {
                            success_style()
                        } else {
                            value_style()
                        },
                    ),
                ];
                if recommended {
                    spans.push(Span::styled(" · recommended", muted()));
                }
                rows.push(TranscriptRow::text(Line::from(spans)));
            }
        } else if let Some(answer) = decision.answers.get(&question.id) {
            rows.push(TranscriptRow::text(Line::from(vec![
                Span::styled(continuation, muted()),
                Span::styled(truncate(answer, 120), success_style()),
            ])));
        }
    }
    if let Some(answer) = &decision.answer {
        rows.push(TranscriptRow::text(Line::from(vec![
            Span::styled("  answer ", success_style()),
            Span::styled(truncate(answer, 140), value_style()),
        ])));
    }
}

pub(crate) fn decision_question_answered(
    decision: &DecisionView,
    question: &DecisionQuestionView,
) -> bool {
    decision
        .answers
        .get(&question.id)
        .is_some_and(|answer| !answer.trim().is_empty())
}

pub(crate) fn decision_ready(decision: &DecisionView) -> bool {
    decision
        .questions
        .iter()
        .filter(|question| question.required)
        .all(|question| decision_question_answered(decision, question))
}

pub(crate) fn match_choice_option(options: &[String], value: &str) -> Option<String> {
    let value = value.trim();
    options
        .iter()
        .find(|option| option.eq_ignore_ascii_case(value))
        .cloned()
        .or_else(|| {
            let lower = value.to_ascii_lowercase();
            options
                .iter()
                .find(|option| option.to_ascii_lowercase().starts_with(&lower))
                .cloned()
        })
}

pub(crate) fn decision_answer_text(decision: &DecisionView) -> String {
    let title = if decision.title.trim().is_empty() {
        "planning decision"
    } else {
        decision.title.trim()
    };
    let mut lines = vec![format!("Decision answer: {title}")];
    if !decision.reason.trim().is_empty() {
        lines.push(format!(
            "Reason: {}",
            compact_one_line(&decision.reason, 220)
        ));
    }
    lines.push("Answers:".to_string());
    for question in &decision.questions {
        let answer = decision
            .answers
            .get(&question.id)
            .map(|answer| compact_one_line(answer, 220))
            .unwrap_or_else(|| "(skipped)".to_string());
        lines.push(format!(
            "- {}: {}",
            compact_one_line(&question.id, 48),
            answer
        ));
    }
    if !decision.assumptions.is_empty() {
        lines.push("Assumptions shown:".to_string());
        for assumption in decision.assumptions.iter().take(6) {
            lines.push(format!("- {}", compact_one_line(assumption, 220)));
        }
    }
    lines.join("\n")
}

pub(crate) fn workflow_view_started(id: String, title: String, task: String) -> WorkflowRunView {
    WorkflowRunView {
        id,
        title,
        task,
        status: WorkflowViewState::Running,
        phases: Vec::new(),
        summary: String::new(),
        expanded: false,
    }
}

pub(crate) fn workflow_state_from_core(status: WorkflowStatus) -> WorkflowViewState {
    match status {
        WorkflowStatus::Running => WorkflowViewState::Running,
        WorkflowStatus::Succeeded => WorkflowViewState::Succeeded,
        WorkflowStatus::PartiallySucceeded => WorkflowViewState::PartiallySucceeded,
        WorkflowStatus::Failed => WorkflowViewState::Failed,
    }
}

pub(crate) fn append_workflow_rows(
    rows: &mut Vec<TranscriptRow>,
    workflow: &WorkflowRunView,
    context: RenderContext,
) {
    let progress = workflow_progress(workflow);
    let mut header_spans = vec![workflow_state_marker_span(
        workflow.status,
        context.animation_tick,
    )];
    header_spans.push(Span::raw(" "));
    if workflow.status == WorkflowViewState::Running {
        header_spans.extend(light_sweep_spans(
            "workflow",
            context.animation_tick,
            |style| style.add_modifier(Modifier::BOLD),
        ));
    } else {
        header_spans.push(Span::styled("workflow", tool_group_label_style()));
    }
    header_spans.extend([
        Span::styled("  ", muted()),
        Span::styled(truncate(&workflow.title, 56), value_style()),
        Span::styled("  ·  ", muted()),
        Span::styled(workflow_progress_label(progress), muted()),
        Span::styled("  ·  ", muted()),
        Span::styled(
            workflow_state_label(workflow.status),
            workflow_state_style(workflow.status),
        ),
    ]);
    rows.push(TranscriptRow::text(Line::from(header_spans)));

    for phase in workflow.phases.iter().take(5) {
        let phase_progress = workflow_phase_progress(phase);
        rows.push(TranscriptRow::text(Line::from(vec![
            Span::styled("   ", muted()),
            workflow_state_marker_span(phase.status, context.animation_tick),
            Span::raw(" "),
            Span::styled(phase.name.clone(), workflow_state_style(phase.status)),
            Span::styled("  ", tool_group_meta_style()),
            Span::styled(phase_progress, muted()),
            Span::styled("  ·  ", muted()),
            Span::styled(truncate(&phase.objective, 70), muted()),
        ])));

        append_workflow_agent_rows(rows, phase, context);
    }

    if !workflow.summary.trim().is_empty() {
        let preview = workflow
            .summary
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or(workflow.summary.trim());
        rows.push(TranscriptRow::text(Line::from(vec![
            Span::styled("      summary ", tool_group_meta_style()),
            Span::styled(truncate(preview.trim(), 140), muted()),
        ])));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WorkflowProgress {
    pub(crate) phases: usize,
    pub(crate) agents: usize,
    pub(crate) succeeded: usize,
    pub(crate) partial: usize,
    pub(crate) failed: usize,
    pub(crate) running: usize,
    pub(crate) pending: usize,
}

pub(crate) fn workflow_progress(workflow: &WorkflowRunView) -> WorkflowProgress {
    let mut progress = WorkflowProgress {
        phases: workflow.phases.len(),
        agents: 0,
        succeeded: 0,
        partial: 0,
        failed: 0,
        running: 0,
        pending: 0,
    };

    for agent in workflow.phases.iter().flat_map(|phase| &phase.agents) {
        progress.agents += 1;
        match agent.status {
            WorkflowViewState::Pending => progress.pending += 1,
            WorkflowViewState::Running => progress.running += 1,
            WorkflowViewState::Succeeded => progress.succeeded += 1,
            WorkflowViewState::PartiallySucceeded => progress.partial += 1,
            WorkflowViewState::Failed => progress.failed += 1,
        }
    }

    progress
}

pub(crate) fn workflow_phase_progress(phase: &WorkflowPhaseView) -> String {
    let agents = phase.agents.len();
    if agents == 0 {
        return "no agents".to_string();
    }

    let succeeded = phase
        .agents
        .iter()
        .filter(|agent| agent.status == WorkflowViewState::Succeeded)
        .count();
    let partial = phase
        .agents
        .iter()
        .filter(|agent| agent.status == WorkflowViewState::PartiallySucceeded)
        .count();
    let failed = phase
        .agents
        .iter()
        .filter(|agent| agent.status == WorkflowViewState::Failed)
        .count();
    let running = phase
        .agents
        .iter()
        .filter(|agent| agent.status == WorkflowViewState::Running)
        .count();

    if running > 0 {
        format!(
            "{running} running · {}/{} complete",
            succeeded + partial,
            agents
        )
    } else if failed > 0 {
        format!(
            "{failed} failed · {}/{} complete",
            succeeded + partial,
            agents
        )
    } else {
        format!("{}/{} complete", succeeded + partial, agents)
    }
}

pub(crate) fn workflow_progress_label(progress: WorkflowProgress) -> String {
    if progress.agents == 0 {
        return format!("{} phases", progress.phases);
    }

    let completed = progress.succeeded + progress.partial;
    if progress.running > 0 {
        format!(
            "{completed}/{} agents · {} running",
            progress.agents, progress.running
        )
    } else if progress.failed > 0 && completed > 0 {
        format!(
            "{completed}/{} agents · {} failed",
            progress.agents, progress.failed
        )
    } else if progress.failed > 0 {
        format!("{}/{} agents failed", progress.failed, progress.agents)
    } else {
        format!("{completed}/{} agents", progress.agents)
    }
}

pub(crate) fn workflow_latest_activity(workflow: &WorkflowRunView) -> String {
    if let Some(agent) = workflow
        .phases
        .iter()
        .flat_map(|phase| &phase.agents)
        .find(|agent| agent.status == WorkflowViewState::Running)
    {
        return format!(
            "{} running · {}",
            agent.name,
            workflow_agent_tool_summary(agent)
        );
    }

    if let Some(agent) = workflow
        .phases
        .iter()
        .flat_map(|phase| &phase.agents)
        .rev()
        .find(|agent| agent.status == WorkflowViewState::Failed)
    {
        return format!(
            "{} failed · {}",
            agent.name,
            workflow_agent_output_preview(agent).unwrap_or_else(|| agent.role.clone())
        );
    }

    if let Some(preview) = workflow
        .summary
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
    {
        return compact_one_line(preview, 80);
    }

    "waiting for events".to_string()
}

pub(crate) fn workflow_activity_style(workflow: &WorkflowRunView) -> Style {
    if workflow.status == WorkflowViewState::Failed
        || workflow
            .phases
            .iter()
            .flat_map(|phase| &phase.agents)
            .any(|agent| agent.status == WorkflowViewState::Failed)
    {
        error_style()
    } else if workflow.status == WorkflowViewState::Running {
        tool_label_style()
    } else {
        muted()
    }
}

pub(crate) fn append_workflow_agent_rows(
    rows: &mut Vec<TranscriptRow>,
    phase: &WorkflowPhaseView,
    context: RenderContext,
) {
    let salient_agents = workflow_salient_agents(phase);
    let hidden = phase.agents.len().saturating_sub(salient_agents.len());

    for agent in salient_agents {
        rows.push(TranscriptRow::text(workflow_agent_line(agent, context)));
        if agent.status == WorkflowViewState::Failed
            && let Some(preview) = workflow_agent_output_preview(agent)
        {
            rows.push(TranscriptRow::text(Line::from(vec![
                Span::styled("         ", muted()),
                Span::styled(truncate(&preview, 126), error_preview_style()),
            ])));
        }
    }

    if hidden > 0 {
        rows.push(TranscriptRow::text(Line::from(vec![
            Span::styled("      ", muted()),
            Span::styled(format!("+{hidden} more agents"), muted()),
        ])));
    }
}

pub(crate) fn workflow_salient_agents(phase: &WorkflowPhaseView) -> Vec<&WorkflowAgentView> {
    let mut agents = phase
        .agents
        .iter()
        .filter(|agent| {
            matches!(
                agent.status,
                WorkflowViewState::Running | WorkflowViewState::Failed
            )
        })
        .take(3)
        .collect::<Vec<_>>();

    if agents.is_empty()
        && matches!(
            phase.status,
            WorkflowViewState::Succeeded | WorkflowViewState::PartiallySucceeded
        )
    {
        agents.extend(
            phase
                .agents
                .iter()
                .filter(|agent| {
                    matches!(
                        agent.status,
                        WorkflowViewState::Succeeded | WorkflowViewState::PartiallySucceeded
                    )
                })
                .take(1),
        );
    }

    agents
}

pub(crate) fn workflow_agent_line(
    agent: &WorkflowAgentView,
    context: RenderContext,
) -> Line<'static> {
    let state_style = workflow_state_style(agent.status);
    Line::from(vec![
        Span::styled("      |-- ", tool_group_meta_style()),
        workflow_state_marker_span(agent.status, context.animation_tick),
        Span::raw(" "),
        Span::styled(agent.name.clone(), state_style),
        Span::styled(
            format!(" [{}]", subagent_tool_policy_label(agent.tool_policy)),
            muted(),
        ),
        Span::styled("  ", muted()),
        Span::styled(
            workflow_agent_tool_summary(agent),
            message_style(ChatRole::Tool),
        ),
        Span::styled("  ", muted()),
        Span::styled(workflow_state_label(agent.status), state_style),
    ])
}

pub(crate) fn workflow_agent_tool_summary(agent: &WorkflowAgentView) -> String {
    if agent.tool_counts.is_empty() {
        return agent.role.clone();
    }

    agent
        .tool_counts
        .iter()
        .take(4)
        .map(|(name, count)| {
            let name = tool_display_name(name);
            if *count == 1 {
                name.to_string()
            } else {
                format!("{name} x{count}")
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

pub(crate) fn workflow_agent_output_preview(agent: &WorkflowAgentView) -> Option<String> {
    agent
        .output
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && *line != "done")
        .map(|line| compact_one_line(line, 140))
}

pub(crate) fn workflow_state_marker_span(
    state: WorkflowViewState,
    animation_tick: u64,
) -> Span<'static> {
    match state {
        WorkflowViewState::Pending => Span::styled("·", muted()),
        WorkflowViewState::Running => {
            let frame = animation::ThrobberKind::BrailleOrbit.frame(animation_tick);
            Span::styled(frame.symbol, tool_pulse_style(frame))
        }
        WorkflowViewState::Succeeded => Span::styled("✓", success_style()),
        WorkflowViewState::PartiallySucceeded => Span::styled("◐", prompt_style()),
        WorkflowViewState::Failed => Span::styled("×", error_style()),
    }
}

pub(crate) fn workflow_state_label(state: WorkflowViewState) -> &'static str {
    match state {
        WorkflowViewState::Pending => "queued",
        WorkflowViewState::Running => "running",
        WorkflowViewState::Succeeded => "complete",
        WorkflowViewState::PartiallySucceeded => "partial",
        WorkflowViewState::Failed => "failed",
    }
}

pub(crate) fn subagent_tool_policy_label(policy: SubagentToolPolicy) -> &'static str {
    match policy {
        SubagentToolPolicy::ReadOnly => "read",
        SubagentToolPolicy::ShellRead => "shell-read",
        SubagentToolPolicy::Edit => "edit",
        SubagentToolPolicy::Verify => "verify",
    }
}

pub(crate) fn workflow_state_style(state: WorkflowViewState) -> Style {
    match state {
        WorkflowViewState::Pending => muted(),
        WorkflowViewState::Running => tool_label_style(),
        WorkflowViewState::Succeeded => success_style(),
        WorkflowViewState::PartiallySucceeded => prompt_style(),
        WorkflowViewState::Failed => error_style(),
    }
}

#[cfg(test)]
pub(crate) fn visible_transcript_lines(
    transcript: &[TranscriptItem],
    streaming_message: Option<usize>,
    selected_tool: Option<usize>,
) -> Vec<Line<'static>> {
    transcript_lines_from_rows(&visible_transcript_rows(
        transcript,
        streaming_message,
        selected_tool,
        RenderContext::static_view(),
    ))
}

pub(crate) fn visible_transcript_rows(
    transcript: &[TranscriptItem],
    streaming_message: Option<usize>,
    selected_tool: Option<usize>,
    context: RenderContext,
) -> Vec<TranscriptRow> {
    if transcript.is_empty() {
        return launch_rows();
    }

    let mut rows = Vec::new();
    if should_preserve_launch_rows(transcript, streaming_message) {
        rows.extend(launch_rows());
        rows.push(TranscriptRow::text(Line::from("")));
    }
    let mut index = 0;
    while index < transcript.len() {
        match &transcript[index] {
            TranscriptItem::Message(message) if message.role == ChatRole::Assistant => {
                let assistant = message.clone();
                let assistant_index = index;
                index += 1;

                let activity_start = index;
                while index < transcript.len()
                    && matches!(
                        transcript[index],
                        TranscriptItem::Tool(_) | TranscriptItem::Reasoning(_)
                    )
                {
                    index += 1;
                }

                append_activity_rows(
                    &mut rows,
                    transcript,
                    activity_start,
                    index,
                    selected_tool,
                    context,
                );
                append_chat_message_rows(
                    &mut rows,
                    &assistant,
                    streaming_message == Some(assistant_index),
                );
            }
            TranscriptItem::Message(message) => {
                let is_streaming = streaming_message == Some(index);
                if !rows.is_empty() && message.role == ChatRole::User {
                    rows.push(TranscriptRow::text(Line::from("")));
                }
                append_chat_message_rows(&mut rows, message, is_streaming);
                index += 1;
                if message.role == ChatRole::User {
                    rows.push(TranscriptRow::text(Line::from("")));
                }
            }
            TranscriptItem::Workflow(workflow) => {
                if !rows.is_empty() {
                    rows.push(TranscriptRow::text(Line::from("")));
                }
                append_workflow_rows(&mut rows, workflow, context);
                index += 1;
            }
            // Plans render in the live strip above the composer, not in the
            // transcript; the item stays only as state (persistence + strip).
            TranscriptItem::Plan(_) => {
                index += 1;
            }
            TranscriptItem::Decision(decision) => {
                if !rows.is_empty() {
                    rows.push(TranscriptRow::text(Line::from("")));
                }
                append_decision_rows(&mut rows, decision, context.decision_selection);
                index += 1;
            }
            TranscriptItem::Tool(_) | TranscriptItem::Reasoning(_) => {
                let activity_start = index;
                while index < transcript.len()
                    && matches!(
                        transcript[index],
                        TranscriptItem::Tool(_) | TranscriptItem::Reasoning(_)
                    )
                {
                    index += 1;
                }
                append_activity_rows(
                    &mut rows,
                    transcript,
                    activity_start,
                    index,
                    selected_tool,
                    context,
                );
            }
        }
    }

    append_chat_bottom_padding(&mut rows);
    rows
}

pub(crate) fn append_chat_bottom_padding(rows: &mut Vec<TranscriptRow>) {
    if rows.is_empty() {
        return;
    }

    rows.extend((0..CHAT_BOTTOM_PADDING_ROWS).map(|_| TranscriptRow::text(Line::from(""))));
}

pub(crate) fn should_preserve_launch_rows(
    transcript: &[TranscriptItem],
    streaming_message: Option<usize>,
) -> bool {
    let Some(streaming_index) = streaming_message else {
        return false;
    };
    if transcript.len() > 2 {
        return false;
    }
    let Some(TranscriptItem::Message(first)) = transcript.first() else {
        return false;
    };
    if first.role != ChatRole::User {
        return false;
    }

    matches!(
        transcript.get(streaming_index),
        Some(TranscriptItem::Message(ChatMessage {
            role: ChatRole::Assistant,
            content,
            attachments,
        })) if content.is_empty() && attachments.is_empty()
    )
}

pub(crate) const WORDMARK_WIDE_MIN_COLUMNS: u16 = 64;

pub(crate) fn launch_rows() -> Vec<TranscriptRow> {
    let wide = crossterm::terminal::size()
        .map(|(width, _)| width >= WORDMARK_WIDE_MIN_COLUMNS)
        .unwrap_or(false);

    let mut lines = vec![Line::from("")];
    if wide {
        for art in [
            "  ███╗   ███╗███████╗██████╗ ██╗   ██╗███████╗ █████╗ ",
            "  ████╗ ████║██╔════╝██╔══██╗██║   ██║██╔════╝██╔══██╗",
            "  ██╔████╔██║█████╗  ██║  ██║██║   ██║███████╗███████║",
            "  ██║╚██╔╝██║██╔══╝  ██║  ██║██║   ██║╚════██║██╔══██║",
            "  ██║ ╚═╝ ██║███████╗██████╔╝╚██████╔╝███████║██║  ██║",
        ] {
            lines.push(Line::from(Span::styled(
                art,
                accent().add_modifier(Modifier::BOLD),
            )));
        }
        lines.push(Line::from(vec![
            Span::styled(
                "  ╚═╝     ╚═╝╚══════╝╚═════╝  ╚═════╝ ╚══════╝╚═╝  ╚═╝",
                accent().add_modifier(Modifier::BOLD),
            ),
            Span::styled(concat!("  v", env!("CARGO_PKG_VERSION")), muted()),
        ]));
    } else {
        lines.push(Line::from(Span::styled(
            "  █▀▄▀█ █▀▀ █▀▄ █░█ █▀ ▄▀█",
            accent().add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(vec![
            Span::styled(
                "  █░▀░█ ██▄ █▄▀ █▄█ ▄█ █▀█",
                accent().add_modifier(Modifier::BOLD),
            ),
            Span::styled(concat!("  v", env!("CARGO_PKG_VERSION")), muted()),
        ]));
    }

    lines.extend([
        Line::from(""),
        Line::from(vec![Span::styled(
            "  the coding agent that plans, edits, and verifies",
            value_style(),
        )]),
        Line::from(""),
        Line::from(vec![
            Span::styled("  enter", prompt_style()),
            Span::styled(" send a task", muted()),
            Span::styled("      shift+tab", prompt_style()),
            Span::styled(" plan mode", muted()),
            Span::styled("      ctrl+p", prompt_style()),
            Span::styled(" commands", muted()),
        ]),
        Line::from(vec![
            Span::styled("  ctrl+i", prompt_style()),
            Span::styled(" paste image", muted()),
            Span::styled("    /workflow", prompt_style()),
            Span::styled(" agent fleet", muted()),
            Span::styled("     esc esc", prompt_style()),
            Span::styled(" quit", muted()),
        ]),
        Line::from(""),
    ]);

    lines.into_iter().map(TranscriptRow::text).collect()
}

pub(crate) fn transcript_lines_from_rows(rows: &[TranscriptRow]) -> Vec<Line<'static>> {
    rows.iter().map(|row| row.line.clone()).collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TranscriptImagePlacement {
    pub(crate) attachment: ImageAttachment,
    pub(crate) width: u16,
    pub(crate) height: u16,
    pub(crate) x_offset: u16,
    pub(crate) y_offset: i16,
}

pub(crate) fn transcript_image_placements(
    rows: &[TranscriptRow],
    area: Rect,
    top_offset: usize,
) -> Vec<TranscriptImagePlacement> {
    if area.width < 8 || area.height == 0 {
        return Vec::new();
    }

    let x_offset = 2;
    let image_width = CHAT_IMAGE_PREVIEW_WIDTH.min(area.width.saturating_sub(x_offset));
    if image_width == 0 {
        return Vec::new();
    }

    let viewport_bottom = top_offset.saturating_add(area.height as usize);
    let mut placements = Vec::new();
    let mut visual_start = 0usize;

    for row in rows {
        if let Some(attachment) = &row.image {
            let image_height = CHAT_IMAGE_PREVIEW_HEIGHT;
            let image_bottom = visual_start.saturating_add(image_height as usize);
            if image_bottom > top_offset && visual_start < viewport_bottom {
                placements.push(TranscriptImagePlacement {
                    attachment: attachment.clone(),
                    width: image_width,
                    height: image_height,
                    x_offset,
                    y_offset: signed_visual_offset(visual_start, top_offset),
                });
            }
        }

        visual_start = visual_start.saturating_add(row_visual_height(row, area.width));
        if visual_start >= viewport_bottom {
            break;
        }
    }

    placements
}

pub(crate) fn signed_visual_offset(visual_start: usize, top_offset: usize) -> i16 {
    if visual_start >= top_offset {
        visual_start
            .saturating_sub(top_offset)
            .min(i16::MAX as usize) as i16
    } else {
        -(top_offset
            .saturating_sub(visual_start)
            .min(i16::MAX as usize) as i16)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct TranscriptViewportWindow {
    pub(crate) rows: Vec<TranscriptRow>,
    pub(crate) scroll_offset: usize,
}

pub(crate) fn transcript_viewport_window(
    rows: &[TranscriptRow],
    width: u16,
    top_offset: usize,
    viewport_height: usize,
) -> TranscriptViewportWindow {
    if rows.is_empty() || viewport_height == 0 {
        return TranscriptViewportWindow {
            rows: Vec::new(),
            scroll_offset: 0,
        };
    }

    let mut skipped_visual_rows = 0usize;
    let mut visible_visual_rows = 0usize;
    let mut scroll_offset = 0usize;
    let mut visible_rows = Vec::new();
    let mut taking = false;

    for row in rows {
        let visual_rows = row_visual_height(row, width);
        if !taking {
            if skipped_visual_rows.saturating_add(visual_rows) <= top_offset {
                skipped_visual_rows = skipped_visual_rows.saturating_add(visual_rows);
                continue;
            }
            scroll_offset = top_offset.saturating_sub(skipped_visual_rows);
            taking = true;
        }

        visible_rows.push(row.clone());
        visible_visual_rows = visible_visual_rows.saturating_add(visual_rows);
        if visible_visual_rows.saturating_sub(scroll_offset) >= viewport_height {
            break;
        }
    }

    TranscriptViewportWindow {
        rows: visible_rows,
        scroll_offset,
    }
}

pub(crate) fn chat_viewport_metrics(
    rows: &[TranscriptRow],
    area: Rect,
    requested_scroll: usize,
) -> ChatViewportMetrics {
    let text_area = area;
    let total_visual_lines = wrapped_row_count(rows, text_area.width);
    let has_scrollbar = area.width > 4 && total_visual_lines > area.height as usize;
    let max_scroll = total_visual_lines.saturating_sub(area.height as usize);
    let scroll = requested_scroll.min(max_scroll);
    let top_offset = max_scroll.saturating_sub(scroll);

    ChatViewportMetrics {
        text_area,
        has_scrollbar,
        total_visual_lines,
        max_scroll,
        scroll,
        top_offset,
    }
}

pub(crate) fn scroll_progress_percent(metrics: &ChatViewportMetrics) -> usize {
    if metrics.max_scroll == 0 {
        return 100;
    }
    metrics.top_offset.saturating_mul(100) / metrics.max_scroll
}

pub(crate) fn paragraph_scroll_offset(top_offset: usize) -> u16 {
    top_offset.min(u16::MAX as usize) as u16
}

pub(crate) fn wrapped_row_count(rows: &[TranscriptRow], width: u16) -> usize {
    rows.iter().map(|row| row_visual_height(row, width)).sum()
}

pub(crate) fn row_visual_height(row: &TranscriptRow, width: u16) -> usize {
    let width = width.max(1) as usize;
    if row.image.is_some() {
        1
    } else {
        line_width(&row.line).max(1).div_ceil(width)
    }
}

#[cfg(test)]
pub(crate) fn trim_wrapped_lines_for_viewport(
    rows: &[TranscriptRow],
    width: u16,
    skip_rows: usize,
    viewport_height: usize,
) -> Vec<TranscriptRow> {
    transcript_viewport_window(rows, width, skip_rows, viewport_height).rows
}

pub(crate) fn line_width(line: &Line<'_>) -> usize {
    line.spans
        .iter()
        .map(|span| span.content.chars().map(char_display_width).sum::<usize>())
        .sum()
}

pub(crate) fn char_display_width(ch: char) -> usize {
    if ch == '\n' || ch == '\r' || ch == '\t' {
        1
    } else if ch.is_control() {
        0
    } else {
        1
    }
}

pub(crate) fn input_display_lines(
    input: &str,
    cursor: usize,
    max_lines: usize,
) -> Vec<Line<'static>> {
    if input.is_empty() {
        return vec![Line::from(vec![
            Span::styled("█", cursor_style()),
            Span::styled(" Type a task or ask a question…", placeholder_style()),
        ])];
    }

    let max_lines = max_lines.max(1);
    let visible_start_line = cursor_line(input, cursor)
        .saturating_add(1)
        .saturating_sub(max_lines);

    let mut lines = Vec::new();
    let mut current = Vec::new();
    let mut current_line = 0usize;

    for (index, ch) in input.chars().enumerate() {
        if current_line >= visible_start_line && index == cursor {
            current.push(Span::styled("█", cursor_style()));
        }

        if ch == '\n' {
            if current_line >= visible_start_line {
                lines.push(Line::from(current));
                current = Vec::new();
                if lines.len() >= max_lines {
                    return lines;
                }
            }
            current_line += 1;
        } else if current_line >= visible_start_line {
            current.push(Span::styled(ch.to_string(), value_style()));
        }
    }

    if current_line >= visible_start_line && cursor == input.chars().count() {
        current.push(Span::styled("█", cursor_style()));
    }

    if current_line >= visible_start_line && lines.len() < max_lines {
        lines.push(Line::from(current));
    }
    lines
}

pub(crate) fn cursor_line(input: &str, cursor: usize) -> usize {
    input.chars().take(cursor).filter(|ch| *ch == '\n').count()
}

pub(crate) fn vertically_center_input_lines(
    mut lines: Vec<Line<'static>>,
    available_content_height: u16,
) -> Vec<Line<'static>> {
    let available = available_content_height as usize;
    if available <= lines.len() {
        return lines;
    }

    let top_padding = (available - lines.len()).div_ceil(2);
    if top_padding == 0 {
        return lines;
    }

    let mut centered = vec![Line::from(""); top_padding];
    centered.append(&mut lines);
    centered
}

pub(crate) fn attachment_strip_line(attachments: &[ImageAttachment]) -> Line<'static> {
    let mut spans = vec![Span::styled("  ", muted())];
    for (index, attachment) in attachments.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw(" "));
        }
        spans.push(Span::styled(
            format!("󰋩 {}", attachment_label(attachment)),
            Style::default()
                .fg(palette().success)
                .bg(palette().inline_code_bg)
                .add_modifier(Modifier::BOLD),
        ));
    }
    spans.push(Span::styled("  ctrl+d detach latest", muted()));
    Line::from(spans)
}

pub(crate) fn composer_attachment_preview_lines(
    attachments: &[ImageAttachment],
    previews: &HashMap<String, Vec<Line<'static>>>,
    available_width: u16,
) -> Vec<Line<'static>> {
    if attachments.is_empty() || available_width == 0 {
        return Vec::new();
    }

    let cell_width = COMPOSER_IMAGE_PREVIEW_WIDTH as usize;
    let gap_width = 1usize;
    let usable_width = available_width.saturating_sub(4) as usize;
    let max_cells = (usable_width + gap_width)
        .checked_div(cell_width + gap_width)
        .unwrap_or(1)
        .max(1);
    let visible_count = attachments.len().min(max_cells);
    let hidden_count = attachments.len().saturating_sub(visible_count);
    let mut rows = vec![Vec::new(); COMPOSER_IMAGE_PREVIEW_HEIGHT as usize];

    for attachment in attachments.iter().take(visible_count) {
        let fallback;
        let preview = if let Some(preview) = previews.get(&attachment.id) {
            preview
        } else {
            fallback = image_preview_lines(attachment, COMPOSER_IMAGE_PREVIEW_WIDTH);
            &fallback
        };

        for (row_index, row) in rows.iter_mut().enumerate() {
            if !row.is_empty() {
                row.push(Span::raw(" "));
            }
            if let Some(line) = preview.get(row_index) {
                row.extend(line.spans.clone());
            } else {
                row.push(Span::raw(" ".repeat(cell_width)));
            }
        }
    }

    if hidden_count > 0
        && let Some(first_row) = rows.first_mut()
    {
        first_row.push(Span::raw(" "));
        first_row.push(Span::styled(
            format!("+{hidden_count}"),
            attachment_preview_meta_style(),
        ));
    }

    rows.into_iter()
        .map(|spans| {
            let mut prefixed = vec![Span::styled("  ", muted())];
            prefixed.extend(spans);
            Line::from(prefixed)
        })
        .collect()
}

pub(crate) fn image_preview_lines(attachment: &ImageAttachment, width: u16) -> Vec<Line<'static>> {
    image_placeholder_lines(attachment, width, COMPOSER_IMAGE_PREVIEW_HEIGHT)
}

pub(crate) fn image_input_warning(provider: &str) -> Option<&'static str> {
    if provider == "codex" {
        None
    } else {
        Some("current backend sends a placeholder instead of image pixels")
    }
}

pub(crate) fn preview_image_dimensions(
    attachment: &ImageAttachment,
    area: Rect,
    zoom: u16,
) -> (u16, u16) {
    if area.width == 0 || area.height == 0 {
        return (0, 0);
    }

    let image_width = attachment.width.max(1) as f64;
    let image_height = attachment.height.max(1) as f64;
    let fit_height_from_width = ((area.width as f64 * image_height / image_width) * 0.5)
        .ceil()
        .max(1.0) as u16;

    let (fit_width, fit_height) = if fit_height_from_width <= area.height {
        (area.width, fit_height_from_width)
    } else {
        let width = ((area.height as f64 * image_width / image_height) * 2.0)
            .ceil()
            .max(1.0) as u16;
        (width.min(area.width), area.height)
    };

    let zoom = zoom.clamp(IMAGE_PREVIEW_MIN_ZOOM, IMAGE_PREVIEW_MAX_ZOOM) as u32;
    let width = ((fit_width as u32).saturating_mul(zoom) / 100).clamp(1, u16::MAX as u32) as u16;
    let height = ((fit_height as u32).saturating_mul(zoom) / 100).clamp(1, u16::MAX as u32) as u16;

    (width, height)
}

pub(crate) fn image_placeholder_lines(
    attachment: &ImageAttachment,
    width: u16,
    height: u16,
) -> Vec<Line<'static>> {
    let width = width.max(10) as usize;
    let height = height.max(3) as usize;
    let inner_width = width.saturating_sub(2);
    let border = "─".repeat(inner_width);
    let dimensions = format!("{}×{}", attachment.width, attachment.height);
    let size = human_bytes(attachment.size_bytes);
    let name = truncate(&attachment.name, inner_width);
    let mut lines = vec![
        Line::from(Span::styled(
            format!("╭{border}╮"),
            attachment_preview_border_style(),
        )),
        attachment_preview_body_line("image", width, attachment_preview_title_style()),
        attachment_preview_body_line(&dimensions, width, attachment_preview_meta_style()),
    ];

    while lines.len() + 2 < height {
        lines.push(attachment_preview_body_line("", width, muted()));
    }

    lines.push(attachment_preview_body_line(
        &format!("{name} {size}"),
        width,
        muted(),
    ));
    lines.push(Line::from(Span::styled(
        format!("╰{border}╯"),
        attachment_preview_border_style(),
    )));
    lines
}

pub(crate) fn attachment_preview_body_line(
    text: &str,
    width: usize,
    style: Style,
) -> Line<'static> {
    let inner_width = width.saturating_sub(2);
    let fitted = truncate(text, inner_width);
    let padding = inner_width.saturating_sub(fitted.chars().count());
    Line::from(vec![
        Span::styled("│", attachment_preview_border_style()),
        Span::styled(fitted, style),
        Span::raw(" ".repeat(padding)),
        Span::styled("│", attachment_preview_border_style()),
    ])
}

pub(crate) fn append_chat_message_rows(
    rows: &mut Vec<TranscriptRow>,
    message: &ChatMessage,
    is_streaming: bool,
) {
    if message.role == ChatRole::User {
        append_user_message_rows(rows, &message.content, &message.attachments);
        return;
    }

    let mut lines = Vec::new();
    append_chat_message_lines(&mut lines, message, is_streaming);
    rows.extend(lines.into_iter().map(TranscriptRow::text));
}

pub(crate) fn append_chat_message_lines(
    lines: &mut Vec<Line<'static>>,
    message: &ChatMessage,
    is_streaming: bool,
) {
    if message.role == ChatRole::User {
        append_user_message_lines(lines, &message.content, &message.attachments);
        return;
    }

    if message.role == ChatRole::System {
        for line in message
            .content
            .lines()
            .filter(|line| !line.trim().is_empty())
        {
            lines.push(Line::from(vec![
                Span::styled("  · ", muted()),
                Span::styled(line.trim().to_string(), muted()),
            ]));
        }
        return;
    }

    if message.content.is_empty() && is_streaming {
        return;
    }

    let mut rendered = markdown_content_lines(&message.content, message.role);
    if is_streaming {
        if let Some(last) = rendered.last_mut() {
            last.spans.push(Span::styled("█", cursor_style()));
        } else {
            rendered.push(Line::from(Span::styled("  █", cursor_style())));
        }
    }

    lines.extend(rendered);
}

pub(crate) fn append_user_message_rows(
    rows: &mut Vec<TranscriptRow>,
    content: &str,
    attachments: &[ImageAttachment],
) {
    let mut lines = Vec::new();
    append_user_message_lines(&mut lines, content, attachments);
    rows.extend(lines.into_iter().map(TranscriptRow::text));

    for attachment in attachments {
        let preview = image_placeholder_lines(
            attachment,
            CHAT_IMAGE_PREVIEW_WIDTH,
            CHAT_IMAGE_PREVIEW_HEIGHT,
        );
        for (index, line) in preview.into_iter().enumerate() {
            if index == 0 {
                rows.push(TranscriptRow::image(line, attachment.clone()));
            } else {
                rows.push(TranscriptRow::text(line));
            }
        }
    }
}

pub(crate) fn append_user_message_lines(
    lines: &mut Vec<Line<'static>>,
    content: &str,
    attachments: &[ImageAttachment],
) {
    let base_style = user_message_style();
    let prompt_style = user_message_prompt_style();

    if content.trim().is_empty() {
        lines.push(Line::from(vec![
            Span::styled(" › ", prompt_style),
            Span::styled(" ", base_style),
        ]));
        return;
    }

    for (index, raw_line) in content.lines().enumerate() {
        let marker = if index == 0 { " › " } else { "   " };
        let mut spans = vec![Span::styled(marker, prompt_style)];
        spans.extend(inline_markdown_spans(raw_line.trim_end(), base_style));
        spans.push(Span::styled(" ", base_style));
        lines.push(Line::from(spans));
    }

    if !attachments.is_empty() {
        lines.push(attachment_strip_line(attachments).style(user_message_background_style()));
    }
}

pub(crate) fn append_activity_rows(
    rows: &mut Vec<TranscriptRow>,
    transcript: &[TranscriptItem],
    start: usize,
    end: usize,
    selected_tool: Option<usize>,
    context: RenderContext,
) {
    let mut lines = Vec::new();
    append_activity_lines(&mut lines, transcript, start, end, selected_tool, context);
    rows.extend(lines.into_iter().map(TranscriptRow::text));
}

pub(crate) fn append_activity_lines(
    lines: &mut Vec<Line<'static>>,
    transcript: &[TranscriptItem],
    start: usize,
    end: usize,
    selected_tool: Option<usize>,
    context: RenderContext,
) {
    let has_tools = transcript[start..end]
        .iter()
        .any(|item| matches!(item, TranscriptItem::Tool(_)));

    if !has_tools {
        return;
    }

    append_tool_group_lines(lines, transcript, start, end, selected_tool, context);
}

pub(crate) fn tool_group_is_open(transcript: &[TranscriptItem], start: usize, end: usize) -> bool {
    transcript[start..end]
        .iter()
        .find_map(|item| match item {
            TranscriptItem::Tool(run) => Some(run.group_expanded),
            _ => None,
        })
        .unwrap_or(false)
}

/// The tool verb without the redundant leading name, e.g. "read src/main.rs" -> "src/main.rs".
pub(crate) fn tool_summary_rest(run: &ToolRun) -> String {
    let name = tool_display_name(&run.name);
    let summary = tool_summary(&run.summary);
    summary
        .strip_prefix(name)
        .map(str::trim_start)
        .unwrap_or(summary.as_str())
        .to_string()
}

/// Edits and patches carry diffs the user should see per-call; never merge them.
pub(crate) fn tool_name_coalescible(name: &str) -> bool {
    !matches!(name, "file.edit" | "file.patch")
}

/// True when the group contains at least one coalescible run — consecutive
/// succeeded calls to the same tool (reasoning items in between don't break a run).
pub(crate) fn tool_group_has_coalesced_runs(
    transcript: &[TranscriptItem],
    start: usize,
    end: usize,
) -> bool {
    let mut previous: Option<&str> = None;
    for item in &transcript[start..end] {
        match item {
            TranscriptItem::Tool(run)
                if run.state == ToolRunState::Succeeded && tool_name_coalescible(&run.name) =>
            {
                let name = tool_display_name(&run.name);
                if previous == Some(name) {
                    return true;
                }
                previous = Some(name);
            }
            TranscriptItem::Tool(_) => previous = None,
            _ => {}
        }
    }
    false
}

pub(crate) const TOOL_COALESCE_SHOWN_TARGETS: usize = 3;

pub(crate) fn append_coalesced_tool_lines(
    lines: &mut Vec<Line<'static>>,
    runs: &[&ToolRun],
    selected: bool,
    context: RenderContext,
) {
    let selection = if selected {
        activity_selected_style()
    } else {
        Style::default()
    };
    let sel = |style: Style| style.patch(selection);

    let name = tool_display_name(&runs[0].name);
    // A running call can only ever be the tail of a coalesced run.
    let active = runs
        .last()
        .filter(|run| run.state == ToolRunState::Running)
        .copied();
    let targets: Vec<String> = runs
        .iter()
        .filter(|run| run.state == ToolRunState::Succeeded)
        .map(|run| tool_summary_rest(run))
        .filter(|target| !target.is_empty())
        .collect();
    let extra = targets.len().saturating_sub(TOOL_COALESCE_SHOWN_TARGETS);
    let mut label = targets
        .iter()
        .take(TOOL_COALESCE_SHOWN_TARGETS)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if extra > 0 {
        label.push_str(&format!(" +{extra} more"));
    }
    if let Some(active) = active {
        let target = tool_summary_rest(active);
        if !target.is_empty() {
            if !label.is_empty() {
                label.push_str(", ");
            }
            label.push_str(&target);
        }
    }
    if label.is_empty() {
        label = format!("×{}", runs.len());
    }

    let marker = if active.is_some() {
        tool_running_marker_span(ToolRunState::Running, context.animation_tick)
    } else {
        Span::styled(TOOL_MARKER, sel(tool_marker_style()))
    };
    lines.push(Line::from(vec![
        marker,
        Span::raw(" "),
        Span::styled(
            name.to_string(),
            sel(tool_label_style().add_modifier(Modifier::BOLD)),
        ),
        Span::styled(
            format!(" {}", truncate(&label, 140)),
            sel(message_style(ChatRole::Tool)),
        ),
    ]));

    let mut result = vec![
        Span::raw("  "),
        Span::styled("⎿ ", separator_style()),
        Span::styled(format!("{} calls", runs.len()), muted()),
    ];
    if active.is_some() {
        result.push(Span::styled(" · running…", muted()));
    }
    if selected {
        result.push(Span::styled(" · enter to expand", muted()));
    }
    lines.push(Line::from(result));
}

pub(crate) fn append_tool_group_lines(
    lines: &mut Vec<Line<'static>>,
    transcript: &[TranscriptItem],
    start: usize,
    end: usize,
    selected_tool: Option<usize>,
    context: RenderContext,
) {
    let coalesce = !tool_group_is_open(transcript, start, end);
    let mut first = true;
    let mut index = start;
    while index < end {
        let TranscriptItem::Tool(run) = &transcript[index] else {
            index += 1;
            continue;
        };

        // Collect the consecutive run of succeeded calls to the same tool,
        // skipping reasoning items in between.
        let mut matched: Vec<&ToolRun> = vec![run];
        let mut cursor = index + 1;
        if coalesce
            && run.state == ToolRunState::Succeeded
            && !run.expanded
            && tool_name_coalescible(&run.name)
        {
            loop {
                let mut probe = cursor;
                while probe < end && matches!(transcript[probe], TranscriptItem::Reasoning(_)) {
                    probe += 1;
                }
                match transcript.get(probe) {
                    Some(TranscriptItem::Tool(next))
                        if probe < end
                            && next.state == ToolRunState::Succeeded
                            && !next.expanded
                            && tool_display_name(&next.name) == tool_display_name(&run.name) =>
                    {
                        matched.push(next);
                        cursor = probe + 1;
                    }
                    // A running call of the same tool joins as the live tail, so
                    // it doesn't render below only to jump into the run on success.
                    Some(TranscriptItem::Tool(next))
                        if probe < end
                            && next.state == ToolRunState::Running
                            && tool_display_name(&next.name) == tool_display_name(&run.name) =>
                    {
                        matched.push(next);
                        cursor = probe + 1;
                        break;
                    }
                    _ => break,
                }
            }
        }

        if !first {
            lines.push(Line::from(""));
        }
        first = false;

        if matched.len() > 1 {
            let selected = matches!(selected_tool, Some(sel) if sel >= index && sel < cursor);
            append_coalesced_tool_lines(lines, &matched, selected, context);
            index = cursor;
        } else {
            append_tool_call_lines(lines, run, selected_tool == Some(index), context);
            index += 1;
        }
    }
}

pub(crate) const TOOL_DETAIL_COLLAPSED_LINES: usize = 1;
pub(crate) const TOOL_DETAIL_FAILED_LINES: usize = 4;
pub(crate) const TOOL_DETAIL_EXPANDED_LINES: usize = 24;
/// Diffs are the payoff of an edit — show a real chunk of them by default.
pub(crate) const TOOL_DETAIL_DIFF_COLLAPSED_LINES: usize = 12;
pub(crate) const TOOL_DETAIL_DIFF_EXPANDED_LINES: usize = 64;

pub(crate) fn append_tool_call_lines(
    lines: &mut Vec<Line<'static>>,
    run: &ToolRun,
    selected: bool,
    context: RenderContext,
) {
    let selection = if selected {
        activity_selected_style()
    } else {
        Style::default()
    };
    let sel = |style: Style| style.patch(selection);

    let marker = match run.state {
        ToolRunState::Running => tool_running_marker_span(run.state, context.animation_tick),
        ToolRunState::Succeeded => Span::styled(TOOL_MARKER, sel(tool_marker_style())),
        ToolRunState::Failed => Span::styled(TOOL_MARKER, sel(error_style())),
    };

    let mut row = vec![marker, Span::raw(" ")];
    let name = tool_display_name(&run.name);
    // Summaries like "read AGENTS.md" already start with the tool verb, so
    // drop the redundant name to avoid "read read AGENTS.md".
    let summary = tool_summary(&run.summary);
    let summary_rest = summary
        .strip_prefix(name)
        .map(str::trim_start)
        .unwrap_or(summary.as_str());
    row.push(Span::styled(
        name.to_string(),
        sel(tool_label_style().add_modifier(Modifier::BOLD)),
    ));
    if !summary_rest.is_empty() {
        row.push(Span::styled(
            format!(" {}", truncate(summary_rest, 140)),
            sel(message_style(ChatRole::Tool)),
        ));
    }
    lines.push(Line::from(row));

    if run.state == ToolRunState::Running {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled("⎿ ", separator_style()),
            Span::styled("running…", muted()),
        ]));
        return;
    }

    let detail_lines = meaningful_tool_output_lines(run);
    if detail_lines.is_empty() {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled("⎿ ", separator_style()),
            match run.state {
                ToolRunState::Failed => Span::styled("failed", error_style()),
                _ => Span::styled("done", muted()),
            },
        ]));
        return;
    }

    let has_diff = tool_run_has_diff(run);
    let visible = if run.expanded {
        if has_diff {
            TOOL_DETAIL_DIFF_EXPANDED_LINES
        } else {
            TOOL_DETAIL_EXPANDED_LINES
        }
    } else if run.state == ToolRunState::Failed {
        TOOL_DETAIL_FAILED_LINES
    } else if has_diff {
        TOOL_DETAIL_DIFF_COLLAPSED_LINES
    } else {
        TOOL_DETAIL_COLLAPSED_LINES
    };
    let body_style = match run.state {
        ToolRunState::Failed => error_preview_style(),
        _ => muted(),
    };

    for (index, line) in detail_lines.iter().take(visible).enumerate() {
        let prefix = if index == 0 { "⎿ " } else { "  " };
        let mut row = vec![Span::raw("  "), Span::styled(prefix, separator_style())];
        if line.contains('\u{1b}') {
            row.extend(ansi_detail_spans(line, body_style));
        } else {
            let line_style = if has_diff && run.state != ToolRunState::Failed {
                diff_line_style(line).unwrap_or(body_style)
            } else {
                body_style
            };
            row.push(Span::styled(truncate(line, 170), line_style));
        }
        lines.push(Line::from(row));
    }

    let hidden = detail_lines.len().saturating_sub(visible);
    if hidden > 0 {
        let hint = if run.expanded {
            format!(
                "… +{hidden} more line{}",
                if hidden == 1 { "" } else { "s" }
            )
        } else {
            format!(
                "… +{hidden} line{} (enter to expand)",
                if hidden == 1 { "" } else { "s" }
            )
        };
        lines.push(Line::from(vec![
            Span::raw("    "),
            Span::styled(hint, muted()),
        ]));
    }
}

pub(crate) fn tool_running_marker_span(state: ToolRunState, animation_tick: u64) -> Span<'static> {
    match state {
        ToolRunState::Running => {
            let frame = animation::ThrobberKind::ToolPulse.frame(animation_tick);
            Span::styled(frame.symbol, tool_pulse_style(frame))
        }
        ToolRunState::Succeeded => Span::styled("✓", success_style()),
        ToolRunState::Failed => Span::styled("×", error_style()),
    }
}

pub(crate) fn tool_pulse_style(frame: animation::ThrobberFrame) -> Style {
    match frame.energy {
        3 => tool_label_style().add_modifier(Modifier::BOLD),
        2 => Style::default()
            .fg(accent_color())
            .add_modifier(Modifier::BOLD),
        1 => Style::default()
            .fg(accent_color())
            .add_modifier(Modifier::BOLD),
        _ => muted(),
    }
}

pub(crate) fn light_sweep_spans(
    text: &str,
    animation_tick: u64,
    style_patch: impl Fn(Style) -> Style,
) -> Vec<Span<'static>> {
    let chars = text.chars().collect::<Vec<_>>();
    if chars.is_empty() {
        return Vec::new();
    }

    let char_count = chars.len();
    chars
        .into_iter()
        .enumerate()
        .map(|(index, ch)| {
            let distance =
                animation::light_sweep_distance(index, char_count, animation_tick).unwrap_or(0);
            let style = match distance {
                0 => style_patch(tool_label_style().add_modifier(Modifier::BOLD)),
                1..=2 => style_patch(Style::default().fg(accent_color())),
                3..=4 => style_patch(message_style(ChatRole::Tool)),
                _ => style_patch(tool_group_meta_style()),
            };
            Span::styled(ch.to_string(), style)
        })
        .collect()
}

pub(crate) fn tool_display_name(name: &str) -> &str {
    match name {
        "file.read" => "read",
        "file.search" => "search",
        "fs.list" => "list",
        "terminal.exec" => "terminal",
        "file.edit" => "edit",
        "file.patch" => "patch",
        "web.fetch" => "fetch",
        "web.search" => "web",
        "task.update" => "status",
        other => other,
    }
}

pub(crate) fn meaningful_tool_output_lines(run: &ToolRun) -> Vec<String> {
    run.detail
        .lines()
        // trim_end only: leading whitespace is meaningful in diff output.
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty() && line.trim() != "done")
        .map(ToString::to_string)
        .collect()
}

/// True when this run's detail carries a display diff from file.edit/file.patch.
pub(crate) fn tool_run_has_diff(run: &ToolRun) -> bool {
    matches!(run.name.as_str(), "file.edit" | "file.patch")
}

/// Convert a detail line containing ANSI escape codes into styled spans.
/// Unstyled segments fall back to the tool body style so colored fragments
/// (e.g. cargo's red `error`) sit inside otherwise-muted output.
pub(crate) fn ansi_detail_spans(line: &str, fallback: Style) -> Vec<Span<'static>> {
    use ansi_to_tui::IntoText;

    let Ok(text) = line.into_text() else {
        return vec![Span::styled(line.replace('\u{1b}', "␛"), fallback)];
    };
    let Some(parsed) = text.lines.into_iter().next() else {
        return Vec::new();
    };
    parsed
        .spans
        .into_iter()
        .map(|span| {
            // Reset/uncolored segments take the tool body style; ansi-to-tui
            // encodes SGR reset as explicit Color::Reset rather than default.
            let unstyled = match span.style.fg {
                None | Some(Color::Reset) => true,
                Some(_) => false,
            };
            let style = if unstyled { fallback } else { span.style };
            Span::styled(span.content.into_owned(), style)
        })
        .collect()
}

pub(crate) fn diff_line_style(line: &str) -> Option<Style> {
    let trimmed = line.trim_start();
    if trimmed.contains("verify:") && trimmed.contains("FAILED") {
        return Some(error_style());
    }
    if trimmed.starts_with("+") {
        Some(Style::default().fg(palette().success))
    } else if trimmed.starts_with("-") {
        Some(Style::default().fg(palette().error))
    } else if trimmed.starts_with("@@") {
        Some(muted().add_modifier(Modifier::DIM))
    } else {
        None
    }
}

pub(crate) fn tool_output_failed(output: &str) -> bool {
    if output.starts_with("failed") || output.starts_with("error:") {
        return true;
    }

    if let Some(exit) = output.strip_prefix("exit: ") {
        let code = exit.split_whitespace().next().unwrap_or("");
        return code != "0";
    }

    false
}

pub(crate) fn compact_tool_detail(output: &str) -> String {
    if output.trim().is_empty() {
        return "done".to_string();
    }

    let is_raw_terminal = output.starts_with("exit:")
        && output
            .lines()
            .any(|line| matches!(line.trim(), "stdout:" | "stderr:" | "stdout: <empty>"));

    // Edit/patch diffs and pre-summarized terminal output are already compacted
    // upstream and their body is the whole point of the expanded view; keep them whole.
    if !is_raw_terminal
        && (output.starts_with("edited ")
            || output.starts_with("patched ")
            || output.starts_with("exit:"))
    {
        return output.to_string();
    }

    // Raw terminal-format output (background streams): drop section markers.
    if is_raw_terminal
        || output.starts_with("patched files:")
        || output.starts_with("edited files:")
    {
        return output
            .lines()
            .filter(|line| {
                let line = line.trim();
                !line.is_empty()
                    && line != "stdout:"
                    && line != "stderr:"
                    && line != "stdout: <empty>"
            })
            .skip(1)
            .take(40)
            .collect::<Vec<_>>()
            .join("\n")
            .if_empty("done");
    }

    output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .take(40)
        .collect::<Vec<_>>()
        .join("\n")
}
