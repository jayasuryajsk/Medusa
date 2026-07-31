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

mod composer;
mod plan;
mod tools;
mod transcript;
mod workflow;

pub(crate) use composer::*;
pub(crate) use plan::*;
pub(crate) use tools::*;
pub(crate) use transcript::*;
pub(crate) use workflow::*;
