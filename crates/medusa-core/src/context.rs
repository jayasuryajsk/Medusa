use std::sync::{Arc, Mutex};

use color_eyre::eyre::{Result, WrapErr, bail};
use serde_json::{Value, json};

use crate::model::{ConversationMessage, DirectCodexBackend};

const DEFAULT_CONTEXT_MAX_TOKENS: usize = 60_000;
const PROTECTED_RECENT_TOOL_OUTPUTS: usize = 4;
const SUMMARY_MAX_CHARS: usize = 6_000;
const SUMMARY_SOURCE_MESSAGE_MAX_CHARS: usize = 2_000;
const SUMMARY_SOURCE_MAX_CHARS: usize = 32_000;

pub(crate) const PRUNED_TOOL_OUTPUT_NOTE: &str =
    "[tool output pruned to conserve context; re-run the tool if you need it again]";

const SUMMARIZER_INSTRUCTIONS: &str = "You compact conversation history for a coding agent. \
Summarize the conversation into a dense brief that preserves: the user's goals and constraints, \
decisions made and why, files and paths touched and how, commands run and their results, \
unresolved problems, and exact identifiers (function names, flags, versions, error messages). \
Use short bullet lines. Maximum 400 words. Output only the summary, no preamble.";

/// Rough token estimate: ~4 characters per token for English text and code.
pub fn estimate_tokens(text: &str) -> usize {
    text.chars().count().div_ceil(4)
}

pub fn message_tokens(message: &ConversationMessage) -> usize {
    // Small per-message overhead plus a large flat cost per image attachment.
    estimate_tokens(&message.content) + 4 + message.attachments.len() * 1_500
}

pub(crate) fn value_tokens(value: &Value) -> usize {
    estimate_tokens(&value.to_string())
}

/// Estimated token cost of the baseline system prompt Medusa sends with every
/// request (default tool guidance, no skill/project extras). Used by context
/// readouts so the visible budget accounts for instructions the transcript
/// never shows.
pub fn baseline_instructions_tokens(workspace: &std::path::Path) -> usize {
    let instructions = crate::model::schema::medusa_instructions(
        workspace,
        &crate::model::types::ToolLoopState::default(),
        crate::harness::HarnessPolicy::for_user_prompt(""),
        None,
        false,
    );
    estimate_tokens(&instructions)
}

pub fn context_max_tokens() -> usize {
    if let Some(tokens) = std::env::var("MEDUSA_CONTEXT_MAX_TOKENS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value >= 1_000)
    {
        return tokens;
    }

    // Legacy char-based override, converted at ~4 chars per token.
    if let Some(chars) = std::env::var("MEDUSA_CONTEXT_MAX_CHARS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value >= 8_000)
    {
        return chars / 4;
    }

    DEFAULT_CONTEXT_MAX_TOKENS
}

/// Mid-turn context relief: when the growing tool-loop input exceeds the token
/// budget, replace the oldest tool outputs with a stub, keeping the most
/// recent ones intact. Returns how many outputs were pruned.
pub(crate) fn prune_input_tool_outputs(input: &mut [Value], max_tokens: usize) -> usize {
    let mut total: usize = input.iter().map(value_tokens).sum();
    if total <= max_tokens {
        return 0;
    }

    let output_positions = input
        .iter()
        .enumerate()
        .filter(|(_, item)| {
            item.get("type").and_then(Value::as_str) == Some("function_call_output")
        })
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    let prunable = output_positions
        .len()
        .saturating_sub(PROTECTED_RECENT_TOOL_OUTPUTS);

    let mut pruned = 0;
    for &position in &output_positions[..prunable] {
        if total <= max_tokens {
            break;
        }
        let Some(output) = input[position].get("output").and_then(Value::as_str) else {
            continue;
        };
        if output.len() <= PRUNED_TOOL_OUTPUT_NOTE.len() {
            continue;
        }
        let saved =
            estimate_tokens(output).saturating_sub(estimate_tokens(PRUNED_TOOL_OUTPUT_NOTE));
        input[position]["output"] = json!(PRUNED_TOOL_OUTPUT_NOTE);
        total = total.saturating_sub(saved);
        pruned += 1;
    }
    pruned
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionSummary {
    /// Number of leading non-system messages this summary covers.
    pub covers: usize,
    pub text: String,
}

/// Keeps conversation history inside the token budget. When history outgrows
/// the budget, older messages are folded into an LLM-generated summary that is
/// extended incrementally on later turns; recent messages pass through
/// verbatim. Falls back to a plain omission note if summarization fails.
#[derive(Debug, Clone)]
pub struct ContextEngine {
    max_tokens: usize,
    state: Arc<Mutex<Option<CompactionSummary>>>,
}

impl Default for ContextEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl ContextEngine {
    pub fn new() -> Self {
        Self {
            max_tokens: context_max_tokens(),
            state: Arc::new(Mutex::new(None)),
        }
    }

    #[cfg(test)]
    fn with_max_tokens(max_tokens: usize) -> Self {
        Self {
            max_tokens,
            state: Arc::new(Mutex::new(None)),
        }
    }

    /// Forget the accumulated summary (call when history is cleared or a
    /// different session is loaded).
    pub fn reset(&self) {
        *self.state.lock().expect("context engine lock") = None;
    }

    pub fn summary(&self) -> Option<CompactionSummary> {
        self.state.lock().expect("context engine lock").clone()
    }

    pub fn prepare(
        &self,
        messages: &[ConversationMessage],
        backend: &DirectCodexBackend,
        cancel: &crate::cancel::CancelToken,
    ) -> Vec<ConversationMessage> {
        self.prepare_with_summarizer(messages, |prompt| {
            backend.plain_completion(SUMMARIZER_INSTRUCTIONS, prompt, cancel)
        })
    }

    pub(crate) fn prepare_with_summarizer(
        &self,
        messages: &[ConversationMessage],
        summarize: impl Fn(&str) -> Result<String>,
    ) -> Vec<ConversationMessage> {
        let system_prefix_len = messages
            .iter()
            .take_while(|message| message.role == "system")
            .count();
        let (prefix, body) = messages.split_at(system_prefix_len);

        let mut state = self.state.lock().expect("context engine lock");
        // History shrank underneath us (cleared or switched session).
        if state
            .as_ref()
            .is_some_and(|summary| summary.covers > body.len())
        {
            *state = None;
        }

        let prefix_tokens: usize = prefix.iter().map(message_tokens).sum();
        // Reserve room for the system prompt plus tool traffic during the turn.
        let recent_budget = self
            .max_tokens
            .saturating_sub(prefix_tokens)
            .saturating_mul(7)
            / 10;
        let body_tokens: usize = body.iter().map(message_tokens).sum();
        let base_covers = state.as_ref().map(|summary| summary.covers).unwrap_or(0);

        if body_tokens <= recent_budget && base_covers == 0 {
            return messages.to_vec();
        }

        // Find the cut: keep the longest suffix that fits the recent budget,
        // never resurrecting messages an existing summary already covers, and
        // always keeping at least the last two messages verbatim.
        let mut used = 0usize;
        let mut cut = body.len();
        for (index, message) in body.iter().enumerate().rev() {
            let cost = message_tokens(message);
            if used.saturating_add(cost) > recent_budget && body.len() - index > 2 {
                break;
            }
            used = used.saturating_add(cost);
            cut = index;
        }
        let cut = cut.max(base_covers).min(body.len().saturating_sub(1));

        if cut == 0 {
            return messages.to_vec();
        }

        if state
            .as_ref()
            .is_none_or(|summary| summary.covers < cut || summary.text.is_empty())
        {
            let previous = state.as_ref().map(|summary| summary.text.clone());
            let prompt = summary_source(previous.as_deref(), &body[base_covers..cut]);
            match summarize(&prompt) {
                Ok(text) if !text.trim().is_empty() => {
                    *state = Some(CompactionSummary {
                        covers: cut,
                        text: cap_chars(text.trim(), SUMMARY_MAX_CHARS),
                    });
                }
                _ => {
                    // Summarization unavailable: degrade to a plain omission note.
                    let mut result = prefix.to_vec();
                    result.push(omission_note(cut));
                    result.extend_from_slice(&body[cut..]);
                    return result;
                }
            }
        }

        let summary = state.as_ref().expect("summary present after refresh");
        let mut result = prefix.to_vec();
        result.push(summary_message(summary));
        result.extend_from_slice(&body[summary.covers..]);
        result
    }

    /// User-invoked compaction (`/compact`): fold everything except the last
    /// two non-system messages into the running summary regardless of budget.
    /// The summary lands in the same state `prepare` maintains, so later
    /// turns extend it instead of redoing the work. Unlike `prepare`, a
    /// summarizer failure is surfaced as an error, never degraded silently.
    pub fn compact_now(
        &self,
        messages: &[ConversationMessage],
        backend: &DirectCodexBackend,
        cancel: &crate::cancel::CancelToken,
    ) -> Result<ManualCompaction> {
        self.compact_now_with_summarizer(messages, |prompt| {
            backend.plain_completion(SUMMARIZER_INSTRUCTIONS, prompt, cancel)
        })
    }

    pub(crate) fn compact_now_with_summarizer(
        &self,
        messages: &[ConversationMessage],
        summarize: impl Fn(&str) -> Result<String>,
    ) -> Result<ManualCompaction> {
        let system_prefix_len = messages
            .iter()
            .take_while(|message| message.role == "system")
            .count();
        let (prefix, body) = messages.split_at(system_prefix_len);

        let mut state = self.state.lock().expect("context engine lock");
        if state
            .as_ref()
            .is_some_and(|summary| summary.covers > body.len())
        {
            *state = None;
        }
        let base_covers = state.as_ref().map(|summary| summary.covers).unwrap_or(0);

        // Keep the last two messages verbatim, like prepare's floor.
        let cut = body.len().saturating_sub(2);
        if cut <= base_covers {
            bail!("nothing to compact: recent history is already summarized or too short");
        }

        let before_tokens = messages.iter().map(message_tokens).sum::<usize>();
        let previous = state.as_ref().map(|summary| summary.text.clone());
        let prompt = summary_source(previous.as_deref(), &body[base_covers..cut]);
        let text = summarize(&prompt).wrap_err("compaction summarizer failed")?;
        if text.trim().is_empty() {
            bail!("compaction summarizer returned no summary");
        }

        let summary = CompactionSummary {
            covers: cut,
            text: cap_chars(text.trim(), SUMMARY_MAX_CHARS),
        };
        let after_tokens = prefix
            .iter()
            .map(message_tokens)
            .chain(std::iter::once(message_tokens(&summary_message(&summary))))
            .chain(body[cut..].iter().map(message_tokens))
            .sum::<usize>();
        let folded_messages = cut - base_covers;
        *state = Some(summary);

        Ok(ManualCompaction {
            before_tokens,
            after_tokens,
            folded_messages,
        })
    }
}

/// Result of a user-invoked `/compact`, in estimated tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ManualCompaction {
    pub before_tokens: usize,
    pub after_tokens: usize,
    pub folded_messages: usize,
}

fn summary_message(summary: &CompactionSummary) -> ConversationMessage {
    ConversationMessage {
        role: "system".to_string(),
        content: format!(
            "Earlier conversation summary (auto-compacted from {} older messages; inspect the workspace with tools when exact state matters):\n{}",
            summary.covers, summary.text
        ),
        attachments: Vec::new(),
    }
}

fn cap_chars(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let capped = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{capped}…")
    } else {
        capped
    }
}

fn omission_note(omitted: usize) -> ConversationMessage {
    ConversationMessage {
        role: "system".to_string(),
        content: format!(
            "Medusa context compaction omitted {omitted} older transcript messages. Continue from the visible recent context; inspect files with file_read, file_search, or fs_list when exact state matters."
        ),
        attachments: Vec::new(),
    }
}

fn summary_source(previous: Option<&str>, messages: &[ConversationMessage]) -> String {
    let mut source = String::new();
    if let Some(previous) = previous {
        source.push_str("Existing summary of even earlier conversation (fold into the update):\n");
        source.push_str(previous);
        source.push_str("\n\nNew messages to fold in:\n");
    } else {
        source.push_str("Conversation to summarize:\n");
    }

    for message in messages {
        if source.len() > SUMMARY_SOURCE_MAX_CHARS {
            source.push_str("[remaining messages truncated]\n");
            break;
        }
        source.push_str(&message.role);
        source.push_str(": ");
        let mut chars = message.content.chars();
        source.extend(chars.by_ref().take(SUMMARY_SOURCE_MESSAGE_MAX_CHARS));
        if chars.next().is_some() {
            source.push_str(" […]");
        }
        source.push('\n');
    }
    source
}

#[cfg(test)]
mod tests {
    use super::*;
    use color_eyre::eyre::eyre;

    fn message(role: &str, content: &str) -> ConversationMessage {
        ConversationMessage {
            role: role.to_string(),
            content: content.to_string(),
            attachments: Vec::new(),
        }
    }

    fn long_text(tokens: usize) -> String {
        "word ".repeat(tokens * 4 / 5)
    }

    #[test]
    fn token_estimates_scale_with_length() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcdefgh"), 2);
        assert!(message_tokens(&message("user", "hello there")) >= 3);
    }

    #[test]
    fn prune_replaces_oldest_tool_outputs_first() {
        let big = "x".repeat(4_000);
        let mut input = vec![
            json!({"role": "user", "content": "task"}),
            json!({"type": "function_call", "call_id": "1", "name": "file_read", "arguments": "{}"}),
            json!({"type": "function_call_output", "call_id": "1", "output": big.clone()}),
            json!({"type": "function_call_output", "call_id": "2", "output": big.clone()}),
            json!({"type": "function_call_output", "call_id": "3", "output": big.clone()}),
            json!({"type": "function_call_output", "call_id": "4", "output": big.clone()}),
            json!({"type": "function_call_output", "call_id": "5", "output": big.clone()}),
            json!({"type": "function_call_output", "call_id": "6", "output": big.clone()}),
        ];

        let pruned = prune_input_tool_outputs(&mut input, 3_000);

        assert_eq!(pruned, 2);
        assert_eq!(input[2]["output"], json!(PRUNED_TOOL_OUTPUT_NOTE));
        assert_eq!(input[3]["output"], json!(PRUNED_TOOL_OUTPUT_NOTE));
        // The most recent four outputs are protected even while over budget.
        for item in &input[4..] {
            assert_eq!(item["output"], json!(big.clone()));
        }
    }

    #[test]
    fn prune_is_noop_under_budget() {
        let mut input =
            vec![json!({"type": "function_call_output", "call_id": "1", "output": "short"})];
        assert_eq!(prune_input_tool_outputs(&mut input, 10_000), 0);
        assert_eq!(input[0]["output"], json!("short"));
    }

    #[test]
    fn small_histories_pass_through_untouched() {
        let engine = ContextEngine::with_max_tokens(10_000);
        let messages = vec![
            message("system", "permissions"),
            message("user", "hi"),
            message("assistant", "hello"),
        ];

        let prepared =
            engine.prepare_with_summarizer(&messages, |_| panic!("summarizer must not run"));

        assert_eq!(prepared, messages);
        assert!(engine.summary().is_none());
    }

    #[test]
    fn oversized_history_folds_into_summary() {
        let engine = ContextEngine::with_max_tokens(1_000);
        let mut messages = vec![message("system", "permissions")];
        for index in 0..8 {
            messages.push(message("user", &format!("q{index} {}", long_text(200))));
            messages.push(message(
                "assistant",
                &format!("a{index} {}", long_text(200)),
            ));
        }

        let prepared = engine.prepare_with_summarizer(&messages, |prompt| {
            assert!(prompt.contains("q0"));
            Ok("- user asked about q things".to_string())
        });

        let summary = engine.summary().expect("summary created");
        assert!(summary.covers > 0);
        assert_eq!(prepared[0].role, "system");
        assert!(prepared[1].content.contains("Earlier conversation summary"));
        assert!(prepared[1].content.contains("q things"));
        // Recent messages survive verbatim.
        assert_eq!(prepared.len(), 2 + (messages.len() - 1 - summary.covers));
        assert!(prepared.last().unwrap().content.starts_with("a7"));
    }

    #[test]
    fn summary_extends_incrementally_without_resummarizing_covered_span() {
        let engine = ContextEngine::with_max_tokens(1_000);
        let mut messages = vec![message("system", "permissions")];
        for index in 0..8 {
            messages.push(message("user", &format!("q{index} {}", long_text(200))));
        }

        let _ = engine.prepare_with_summarizer(&messages, |_| Ok("first summary".to_string()));
        let first_covers = engine.summary().unwrap().covers;

        for index in 8..12 {
            messages.push(message("user", &format!("q{index} {}", long_text(200))));
        }
        let prepared = engine.prepare_with_summarizer(&messages, |prompt| {
            assert!(prompt.contains("first summary"));
            assert!(
                !prompt.contains("q0 "),
                "already-summarized span must not be resummarized"
            );
            Ok("extended summary".to_string())
        });

        let second = engine.summary().unwrap();
        assert!(second.covers > first_covers);
        assert!(prepared[1].content.contains("extended summary"));
    }

    #[test]
    fn summarizer_failure_degrades_to_omission_note() {
        let engine = ContextEngine::with_max_tokens(1_000);
        let mut messages = vec![message("system", "permissions")];
        for index in 0..8 {
            messages.push(message("user", &format!("q{index} {}", long_text(200))));
        }

        let prepared = engine.prepare_with_summarizer(&messages, |_| Err(eyre!("backend offline")));

        assert!(engine.summary().is_none());
        assert!(prepared[1].content.contains("context compaction omitted"));
        assert!(prepared.last().unwrap().content.contains("q7"));
    }

    #[test]
    fn manual_compact_folds_all_but_the_last_two_messages() {
        let engine = ContextEngine::with_max_tokens(1_000_000);
        let mut messages = vec![message("system", "permissions")];
        for index in 0..6 {
            messages.push(message("user", &format!("q{index} {}", long_text(100))));
        }

        let outcome = engine
            .compact_now_with_summarizer(&messages, |prompt| {
                assert!(prompt.contains("q0"));
                assert!(prompt.contains("q3"));
                assert!(!prompt.contains("q4 "), "kept tail must not be summarized");
                Ok("- dense brief".to_string())
            })
            .unwrap();

        // Even far under budget, /compact folds everything but the tail.
        let summary = engine.summary().expect("summary persisted");
        assert_eq!(summary.covers, 4);
        assert_eq!(outcome.folded_messages, 4);
        assert!(outcome.after_tokens < outcome.before_tokens);

        // The next prepare() builds on the manual summary instead of redoing it.
        let prepared =
            engine.prepare_with_summarizer(&messages, |_| panic!("summary must be reused"));
        assert!(prepared[1].content.contains("dense brief"));
        assert!(prepared.last().unwrap().content.starts_with("q5"));
    }

    #[test]
    fn manual_compact_reports_accurate_before_and_after_estimates() {
        let engine = ContextEngine::with_max_tokens(1_000_000);
        let mut messages = vec![message("system", "permissions")];
        for index in 0..6 {
            messages.push(message("user", &format!("q{index} {}", long_text(100))));
        }

        let outcome = engine
            .compact_now_with_summarizer(&messages, |_| Ok("- brief".to_string()))
            .unwrap();

        let before: usize = messages.iter().map(message_tokens).sum();
        assert_eq!(outcome.before_tokens, before);
        let prepared = engine.prepare_with_summarizer(&messages, |_| panic!("must not run"));
        let after: usize = prepared.iter().map(message_tokens).sum();
        assert_eq!(outcome.after_tokens, after);
    }

    #[test]
    fn manual_compact_refuses_short_or_already_compacted_histories() {
        let engine = ContextEngine::with_max_tokens(1_000_000);
        let short = vec![
            message("system", "permissions"),
            message("user", "hi"),
            message("assistant", "hello"),
        ];
        let error = engine
            .compact_now_with_summarizer(&short, |_| panic!("must not summarize"))
            .unwrap_err();
        assert!(error.to_string().contains("nothing to compact"));

        let mut messages = vec![message("system", "permissions")];
        for index in 0..6 {
            messages.push(message("user", &format!("q{index} {}", long_text(100))));
        }
        engine
            .compact_now_with_summarizer(&messages, |_| Ok("- brief".to_string()))
            .unwrap();
        // Nothing new since the last fold: refuse instead of resummarizing.
        let error = engine
            .compact_now_with_summarizer(&messages, |_| panic!("must not resummarize"))
            .unwrap_err();
        assert!(error.to_string().contains("nothing to compact"));
    }

    #[test]
    fn manual_compact_surfaces_summarizer_failures() {
        let engine = ContextEngine::with_max_tokens(1_000_000);
        let mut messages = vec![message("system", "permissions")];
        for index in 0..6 {
            messages.push(message("user", &format!("q{index} {}", long_text(100))));
        }

        let error = engine
            .compact_now_with_summarizer(&messages, |_| Err(eyre!("backend offline")))
            .unwrap_err();

        assert!(error.to_string().contains("compaction summarizer failed"));
        assert!(
            engine.summary().is_none(),
            "failed compact must not persist state"
        );
    }

    #[test]
    fn history_reset_clears_stale_summary() {
        let engine = ContextEngine::with_max_tokens(1_000);
        let mut messages = vec![message("system", "permissions")];
        for index in 0..8 {
            messages.push(message("user", &format!("q{index} {}", long_text(200))));
        }
        let _ = engine.prepare_with_summarizer(&messages, |_| Ok("summary".to_string()));
        assert!(engine.summary().is_some());

        let fresh = vec![message("system", "permissions"), message("user", "hi")];
        let prepared = engine.prepare_with_summarizer(&fresh, |_| panic!("must not run"));

        assert_eq!(prepared, fresh);
        assert!(engine.summary().is_none());
    }
}
