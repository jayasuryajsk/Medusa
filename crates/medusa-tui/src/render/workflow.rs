use super::*;

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
