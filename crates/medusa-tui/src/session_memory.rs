use std::collections::BTreeMap;
use std::path::Path;

use medusa_core::model::{ConversationAttachment, ConversationMessage};
use medusa_core::permissions::PermissionMode;

use crate::constants::{
    SESSION_MEMORY_MAX_PER_KIND, SESSION_STATE_MAX_FILES, SESSION_STATE_MAX_INTENTS,
    SESSION_STATE_MAX_OUTCOMES, SESSION_STATE_MAX_SYSTEM_NOTES, SESSION_STATE_MAX_TOOLS,
    TURN_INTERRUPTED_NOTE,
};
use crate::render::plan_progress;
use crate::types::{
    BackgroundJobView, ChatMessage, ChatRole, DecisionView, PlanItemStatus, PlanView, ToolRun,
    ToolRunState, TranscriptItem, WorkflowRunView, WorkflowViewState,
};
use crate::util::{compact_one_line, tool_summary};

pub(crate) struct SessionStateRuntime<'a> {
    pub(crate) workspace: &'a str,
    pub(crate) model: &'a str,
    pub(crate) permission_mode: PermissionMode,
    pub(crate) status: &'a str,
    pub(crate) workflows: &'a [WorkflowRunView],
    pub(crate) active_workflows: usize,
    pub(crate) background_jobs: &'a BTreeMap<String, BackgroundJobView>,
}

pub(crate) fn transcript_conversation_message(
    item: &TranscriptItem,
) -> Option<ConversationMessage> {
    match item {
        TranscriptItem::Message(message) => match message.role {
            ChatRole::User => Some(ConversationMessage {
                role: "user".to_string(),
                content: message.content.clone(),
                attachments: message
                    .attachments
                    .iter()
                    .map(|attachment| ConversationAttachment {
                        mime: attachment.mime.clone(),
                        path: attachment.path.clone(),
                    })
                    .collect(),
            }),
            ChatRole::Assistant if !message.content.trim().is_empty() => {
                Some(ConversationMessage {
                    role: "assistant".to_string(),
                    content: message.content.clone(),
                    attachments: Vec::new(),
                })
            }
            // The interruption note re-enters model history so a resumed
            // conversation knows the previous turn was cut short.
            ChatRole::System if message.content == TURN_INTERRUPTED_NOTE => {
                Some(ConversationMessage {
                    role: "system".to_string(),
                    content: message.content.clone(),
                    attachments: Vec::new(),
                })
            }
            _ => None,
        },
        _ => None,
    }
}

pub(crate) fn session_state_context_text(
    transcript: &[TranscriptItem],
    recent_message_count: usize,
    runtime: SessionStateRuntime<'_>,
) -> String {
    let conversation_total = transcript
        .iter()
        .filter(|item| transcript_conversation_message(item).is_some())
        .count();
    let tool_total = transcript
        .iter()
        .filter(|item| matches!(item, TranscriptItem::Tool(_)))
        .count();
    let reasoning_total = transcript
        .iter()
        .filter(|item| matches!(item, TranscriptItem::Reasoning(_)))
        .count();
    let omitted = conversation_total.saturating_sub(recent_message_count);
    let running_jobs = runtime
        .background_jobs
        .values()
        .filter(|job| job.state == ToolRunState::Running)
        .count();

    let mut lines = vec![
        "Medusa rolling session state and semantic memory.".to_string(),
        format!(
            "workspace: {} · model: {} · permissions: {} · status: {}",
            runtime.workspace,
            runtime.model,
            runtime.permission_mode.name(),
            compact_one_line(runtime.status, 120)
        ),
        format!(
            "context window: {recent_message_count}/{conversation_total} recent conversation messages retained; {omitted} older messages summarized; {tool_total} tool rows; {reasoning_total} reasoning traces not replayed verbatim."
        ),
    ];

    append_context_section(
        &mut lines,
        "semantic memory",
        semantic_memory_lines(transcript),
    );
    append_context_section(
        &mut lines,
        "recent user intents",
        recent_user_intents(transcript),
    );
    append_context_section(
        &mut lines,
        "recent assistant outcomes",
        recent_assistant_outcomes(transcript),
    );
    append_context_section(
        &mut lines,
        "session/system notes",
        recent_system_notes(transcript),
    );
    append_context_section(
        &mut lines,
        "tool history",
        recent_tool_summaries(transcript),
    );
    append_context_section(
        &mut lines,
        "changed or referenced files",
        file_mentions(transcript),
    );
    append_context_section(
        &mut lines,
        "workflow state",
        workflow_state_lines(runtime.workflows, runtime.active_workflows),
    );
    append_context_section(
        &mut lines,
        "decision state",
        decision_state_lines(transcript),
    );

    if !runtime.background_jobs.is_empty() {
        lines.push("background jobs:".to_string());
        lines.push(format!(
            "- {} total; {running_jobs} running",
            runtime.background_jobs.len()
        ));
        for job in runtime.background_jobs.values().rev().take(4) {
            lines.push(format!(
                "- {} {:?}: {}",
                compact_one_line(&job.id, 24),
                job.state,
                compact_one_line(&job.command, 100)
            ));
        }
    }

    lines.push(
        "Use semantic memory as durable session state. Use recent messages for exact wording. Inspect files with fs_list/file_search/file_read before relying on exact workspace state or editing.".to_string(),
    );
    lines.join("\n")
}

pub(crate) fn append_context_section(lines: &mut Vec<String>, title: &str, items: Vec<String>) {
    if items.is_empty() {
        return;
    }
    lines.push(format!("{title}:"));
    for item in items {
        lines.push(format!("- {item}"));
    }
}

pub(crate) fn recent_user_intents(transcript: &[TranscriptItem]) -> Vec<String> {
    transcript
        .iter()
        .rev()
        .filter_map(|item| match item {
            TranscriptItem::Message(message)
                if message.role == ChatRole::User && !message.content.trim().is_empty() =>
            {
                Some(compact_one_line(&message.content, 180))
            }
            _ => None,
        })
        .take(SESSION_STATE_MAX_INTENTS)
        .collect()
}

pub(crate) fn recent_assistant_outcomes(transcript: &[TranscriptItem]) -> Vec<String> {
    transcript
        .iter()
        .rev()
        .filter_map(|item| match item {
            TranscriptItem::Message(message)
                if message.role == ChatRole::Assistant && !message.content.trim().is_empty() =>
            {
                Some(compact_one_line(&message.content, 220))
            }
            _ => None,
        })
        .take(SESSION_STATE_MAX_OUTCOMES)
        .collect()
}

pub(crate) fn recent_system_notes(transcript: &[TranscriptItem]) -> Vec<String> {
    transcript
        .iter()
        .rev()
        .filter_map(|item| match item {
            TranscriptItem::Message(message)
                if message.role == ChatRole::System && !message.content.trim().is_empty() =>
            {
                Some(compact_one_line(&message.content, 180))
            }
            _ => None,
        })
        .take(SESSION_STATE_MAX_SYSTEM_NOTES)
        .collect()
}

#[derive(Default)]
pub(crate) struct SemanticMemory {
    preferences: Vec<String>,
    decisions: Vec<String>,
    issues: Vec<String>,
    validations: Vec<String>,
    outcomes: Vec<String>,
}

impl SemanticMemory {
    fn is_full(&self) -> bool {
        self.preferences.len() >= SESSION_MEMORY_MAX_PER_KIND
            && self.decisions.len() >= SESSION_MEMORY_MAX_PER_KIND
            && self.issues.len() >= SESSION_MEMORY_MAX_PER_KIND
            && self.validations.len() >= SESSION_MEMORY_MAX_PER_KIND
            && self.outcomes.len() >= SESSION_MEMORY_MAX_PER_KIND
    }

    fn lines(self) -> Vec<String> {
        let mut lines = Vec::new();
        append_memory_kind(&mut lines, "preference", self.preferences);
        append_memory_kind(&mut lines, "decision", self.decisions);
        append_memory_kind(&mut lines, "issue", self.issues);
        append_memory_kind(&mut lines, "validation", self.validations);
        append_memory_kind(&mut lines, "outcome", self.outcomes);
        lines
    }
}

pub(crate) fn append_memory_kind(lines: &mut Vec<String>, label: &str, items: Vec<String>) {
    for item in items {
        lines.push(format!("{label}: {item}"));
    }
}

pub(crate) fn semantic_memory_lines(transcript: &[TranscriptItem]) -> Vec<String> {
    let mut memory = SemanticMemory::default();

    for item in transcript.iter().rev() {
        match item {
            TranscriptItem::Message(message) => {
                collect_message_memory(message, &mut memory);
            }
            TranscriptItem::Tool(run) => collect_tool_memory(run, &mut memory),
            TranscriptItem::Workflow(workflow) => collect_workflow_memory(workflow, &mut memory),
            TranscriptItem::Plan(plan) => collect_plan_memory(plan, &mut memory),
            TranscriptItem::Decision(decision) => collect_decision_memory(decision, &mut memory),
            TranscriptItem::Reasoning(_) => {}
        }

        if memory.is_full() {
            break;
        }
    }

    memory.lines()
}

pub(crate) fn collect_message_memory(message: &ChatMessage, memory: &mut SemanticMemory) {
    let text = compact_one_line(&message.content, 220);
    if text.trim().is_empty() {
        return;
    }

    let lower = text.to_ascii_lowercase();
    match message.role {
        ChatRole::User => {
            if contains_any(
                &lower,
                &[
                    "i prefer",
                    "prefer ",
                    "i like",
                    "i want",
                    "i don't want",
                    "i dont want",
                    "do not",
                    "don't",
                    "dont ",
                    "never ",
                    "always ",
                    "must ",
                    "has to",
                    "should ",
                    "shouldn't",
                    "shouldnt",
                    "keep it",
                    "we need",
                    "we don't",
                    "we dont",
                ],
            ) {
                push_memory(&mut memory.preferences, text.clone());
            }

            if contains_any(
                &lower,
                &[
                    "we decided",
                    "decided",
                    "let's use",
                    "lets use",
                    "use ratatui",
                    "keep it medusa",
                    "we will",
                    "we'll",
                    "we wont",
                    "we won't",
                ],
            ) {
                push_memory(&mut memory.decisions, text.clone());
            }

            if contains_any(
                &lower,
                &[
                    "broken", "error", "failed", "failing", "doesn't", "doesnt", "can't", "cant ",
                    "stuck", "lag", "bad", "fix ",
                ],
            ) {
                push_memory(&mut memory.issues, text);
            }
        }
        ChatRole::Assistant => {
            if contains_any(
                &lower,
                &[
                    "implemented",
                    "added",
                    "changed",
                    "fixed",
                    "wired",
                    "updated",
                    "validation",
                    "passed",
                    "green",
                ],
            ) {
                push_memory(&mut memory.outcomes, text.clone());
            }

            if contains_any(
                &lower,
                &[
                    "cargo test",
                    "cargo check",
                    "passed",
                    "failed",
                    "validation",
                    "tests",
                ],
            ) {
                push_memory(&mut memory.validations, text);
            }
        }
        ChatRole::System | ChatRole::Tool => {}
    }
}

pub(crate) fn collect_tool_memory(run: &ToolRun, memory: &mut SemanticMemory) {
    let summary = tool_summary(&run.summary);
    let detail = compact_one_line(&run.detail, 180);
    let combined = compact_one_line(&format!("{summary} {detail}"), 220);

    if run.state == ToolRunState::Failed {
        push_memory(
            &mut memory.issues,
            format!("{} failed: {}", run.name, combined),
        );
        return;
    }

    if run.name.contains("patch") || run.name.contains("edit") {
        push_memory(
            &mut memory.outcomes,
            format!("{} changed workspace: {}", run.name, combined),
        );
    }

    let lower = combined.to_ascii_lowercase();
    if contains_any(
        &lower,
        &["cargo test", "cargo check", "passed", "finished", "ok"],
    ) {
        push_memory(
            &mut memory.validations,
            format!("{} succeeded: {}", run.name, combined),
        );
    }
}

pub(crate) fn collect_plan_memory(plan: &PlanView, memory: &mut SemanticMemory) {
    if plan.items.is_empty() {
        return;
    }

    let progress = plan_progress(plan);
    let title = if plan.summary.trim().is_empty() {
        "current plan".to_string()
    } else {
        compact_one_line(&plan.summary, 100)
    };
    push_memory(
        &mut memory.decisions,
        format!(
            "plan: {title} · {} steps · {} done · {} blocked",
            plan.items.len(),
            progress.done,
            progress.blocked
        ),
    );

    for item in plan
        .items
        .iter()
        .filter(|item| item.status == PlanItemStatus::Blocked)
    {
        push_memory(
            &mut memory.issues,
            format!("blocked plan step: {}", compact_one_line(&item.text, 140)),
        );
    }
}

pub(crate) fn collect_decision_memory(decision: &DecisionView, memory: &mut SemanticMemory) {
    let title = if decision.title.trim().is_empty() {
        "planning decision".to_string()
    } else {
        compact_one_line(&decision.title, 100)
    };

    if decision.answered {
        let answer = decision
            .answer
            .as_deref()
            .map(|answer| compact_one_line(answer, 140))
            .unwrap_or_else(|| "answered".to_string());
        push_memory(
            &mut memory.decisions,
            format!("answered decision: {title} · {answer}"),
        );
    } else {
        push_memory(
            &mut memory.issues,
            format!(
                "pending decision: {title} · {} question(s)",
                decision.questions.len()
            ),
        );
    }
}

pub(crate) fn collect_workflow_memory(workflow: &WorkflowRunView, memory: &mut SemanticMemory) {
    let title = compact_one_line(&workflow.title, 80);
    let summary = compact_one_line(
        if workflow.summary.trim().is_empty() {
            &workflow.task
        } else {
            &workflow.summary
        },
        180,
    );

    match workflow.status {
        WorkflowViewState::Succeeded => {
            push_memory(
                &mut memory.outcomes,
                format!("workflow succeeded: {title} · {summary}"),
            );
        }
        WorkflowViewState::PartiallySucceeded => {
            push_memory(
                &mut memory.outcomes,
                format!("workflow partially completed: {title} · {summary}"),
            );
            push_memory(
                &mut memory.issues,
                format!("workflow had failed subagents: {title} · {summary}"),
            );
        }
        WorkflowViewState::Failed => {
            push_memory(
                &mut memory.issues,
                format!("workflow failed: {title} · {summary}"),
            );
        }
        WorkflowViewState::Running | WorkflowViewState::Pending => {}
    }
}

pub(crate) fn push_memory(items: &mut Vec<String>, item: String) {
    if items.len() >= SESSION_MEMORY_MAX_PER_KIND || item.trim().is_empty() {
        return;
    }
    if !items.iter().any(|existing| existing == &item) {
        items.push(item);
    }
}

pub(crate) fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| haystack.contains(needle))
}

pub(crate) fn recent_tool_summaries(transcript: &[TranscriptItem]) -> Vec<String> {
    transcript
        .iter()
        .rev()
        .filter_map(|item| match item {
            TranscriptItem::Tool(run) => Some(format!(
                "{} {}: {}{}",
                run.name,
                session_tool_state_label(run.state),
                compact_one_line(&tool_summary(&run.summary), 140),
                tool_detail_suffix(run)
            )),
            _ => None,
        })
        .take(SESSION_STATE_MAX_TOOLS)
        .collect()
}

pub(crate) fn session_tool_state_label(state: ToolRunState) -> &'static str {
    match state {
        ToolRunState::Running => "running",
        ToolRunState::Succeeded => "succeeded",
        ToolRunState::Failed => "failed",
    }
}

pub(crate) fn tool_detail_suffix(run: &ToolRun) -> String {
    if run.detail.trim().is_empty() {
        return String::new();
    }
    format!(" · {}", compact_one_line(&run.detail, 160))
}

pub(crate) fn file_mentions(transcript: &[TranscriptItem]) -> Vec<String> {
    let mut files = Vec::new();
    for item in transcript.iter().rev() {
        if let TranscriptItem::Tool(run) = item {
            collect_file_mentions_from_text(&run.summary, &mut files);
            collect_file_mentions_from_text(&run.detail, &mut files);
        }
        if files.len() >= SESSION_STATE_MAX_FILES {
            break;
        }
    }
    files
}

pub(crate) fn collect_file_mentions_from_text(text: &str, files: &mut Vec<String>) {
    for token in text.split_whitespace() {
        if files.len() >= SESSION_STATE_MAX_FILES {
            return;
        }
        let token = token.trim_matches(|ch: char| {
            matches!(
                ch,
                '`' | '\'' | '"' | ',' | ':' | ';' | ')' | '(' | '[' | ']' | '{' | '}'
            )
        });
        if is_workspace_file_mention(token) && !files.iter().any(|file| file == token) {
            files.push(token.to_string());
        }
    }
}

pub(crate) fn is_workspace_file_mention(token: &str) -> bool {
    if token.is_empty()
        || token.starts_with("http://")
        || token.starts_with("https://")
        || token.starts_with('$')
        || token.contains("://")
        || token.contains(' ')
    {
        return false;
    }

    let has_path_shape = token.contains('/')
        || token.starts_with(".medusa/")
        || token.starts_with("Cargo.")
        || token.starts_with("README")
        || token.starts_with("Makefile");
    let has_file_shape = Path::new(token).extension().is_some()
        || token.ends_with("Makefile")
        || token.ends_with("Dockerfile");
    has_path_shape && has_file_shape
}

pub(crate) fn workflow_state_lines(
    workflows: &[WorkflowRunView],
    active_workflows: usize,
) -> Vec<String> {
    if workflows.is_empty() && active_workflows == 0 {
        return Vec::new();
    }
    let mut lines = vec![format!(
        "{} total; {active_workflows} active",
        workflows.len()
    )];
    lines.extend(workflows.iter().rev().take(6).map(|workflow| {
        format!(
            "{} {:?}: {}",
            compact_one_line(&workflow.title, 80),
            workflow.status,
            compact_one_line(
                if workflow.summary.trim().is_empty() {
                    &workflow.task
                } else {
                    &workflow.summary
                },
                160,
            )
        )
    }));
    lines
}

pub(crate) fn decision_state_lines(transcript: &[TranscriptItem]) -> Vec<String> {
    transcript
        .iter()
        .rev()
        .filter_map(|item| match item {
            TranscriptItem::Decision(decision) => {
                let title = if decision.title.trim().is_empty() {
                    "planning decision".to_string()
                } else {
                    compact_one_line(&decision.title, 100)
                };
                let status = if decision.answered {
                    "answered"
                } else {
                    "waiting"
                };
                let answer = decision
                    .answer
                    .as_deref()
                    .map(|answer| format!(" · answer: {}", compact_one_line(answer, 120)))
                    .unwrap_or_default();
                Some(format!(
                    "{status}: {title} · {} question(s){answer}",
                    decision.questions.len()
                ))
            }
            _ => None,
        })
        .take(4)
        .collect()
}
