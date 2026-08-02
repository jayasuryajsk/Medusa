use std::path::PathBuf;

use crate::credentials::CredentialStore;
use crate::harness::HarnessPolicy;
use crate::model::provider::{ModelReference, ProviderRegistry};
use crate::orchestrator::TurnOrchestrator;

#[derive(Debug, Clone)]
pub struct ModelGateway {
    pub(crate) workspace: PathBuf,
    pub(crate) registry: ProviderRegistry,
    pub(crate) selection: ModelReference,
    pub(crate) model_reference: String,
    pub(crate) provider_hint: Option<String>,
    pub(crate) reasoning_effort: String,
    pub(crate) credentials: CredentialStore,
    pub(crate) client: reqwest::blocking::Client,
}

/// Compatibility name retained for downstream users while the codebase moves
/// to provider-neutral terminology.
pub type DirectCodexBackend = ModelGateway;

pub(crate) fn is_mutation_tool(name: &str) -> bool {
    matches!(name, "file_edit" | "file_patch")
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelChatResult {
    pub response: String,
    pub event_count: usize,
}

/// Token usage reported by the backend for a single model request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
    /// Cached input tokens (a subset of `input`) when the backend reports them.
    pub cached: u64,
}

impl TokenUsage {
    pub fn total(&self) -> u64 {
        self.input + self.output
    }

    pub fn uncached_input(&self) -> u64 {
        self.input.saturating_sub(self.cached)
    }

    pub fn cache_hit_percent(&self) -> Option<f64> {
        (self.input > 0).then(|| self.cached.min(self.input) as f64 * 100.0 / self.input as f64)
    }

    pub fn add(&mut self, other: TokenUsage) {
        self.input += other.input;
        self.output += other.output;
        self.cached += other.cached;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelStreamEvent {
    Delta(String),
    ReasoningDelta(String),
    CompactionStarted {
        before_tokens: usize,
    },
    CompactionFinished {
        before_tokens: usize,
        after_tokens: usize,
        folded_messages: usize,
    },
    ToolStart {
        call_id: String,
        name: String,
        summary: String,
    },
    ToolResult {
        call_id: String,
        name: String,
        output: String,
    },
    Workflow(crate::workflow::WorkflowEvent),
    /// Backend-reported token usage for one model request. A turn that runs
    /// tools makes several requests, so consumers must sum every Usage event
    /// they see to get turn totals.
    Usage(TokenUsage),
    Done {
        event_count: usize,
    },
    Error(String),
    /// The user interrupted the turn (Esc) — an outcome, not a failure.
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationMessage {
    pub role: String,
    pub content: String,
    pub attachments: Vec<ConversationAttachment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationAttachment {
    pub mime: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone)]
pub(crate) struct ToolCall {
    pub(crate) name: String,
    pub(crate) call_id: String,
    pub(crate) arguments: String,
    pub(crate) reasoning_content: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct ToolLoopState {
    pub(crate) patch_requires_context: bool,
    pub(crate) orchestrator: TurnOrchestrator,
}

impl ToolLoopState {
    pub(crate) fn for_policy(policy: HarnessPolicy) -> Self {
        Self {
            patch_requires_context: false,
            orchestrator: TurnOrchestrator::new(policy),
        }
    }

    pub(crate) fn native_mutation_allowed(&self) -> bool {
        !self.patch_requires_context && self.orchestrator.native_mutation_allowed()
    }
}

impl Default for ToolLoopState {
    fn default() -> Self {
        Self::for_policy(HarnessPolicy::for_user_prompt(""))
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ToolLoopPolicy {
    mutation: bool,
    workflows: bool,
}

impl ToolLoopPolicy {
    pub(crate) fn mutation_allowed() -> Self {
        Self {
            mutation: true,
            workflows: true,
        }
    }

    pub(crate) fn read_only() -> Self {
        Self {
            mutation: false,
            workflows: false,
        }
    }

    /// Workflow subagents never get the workflow tool themselves: one level
    /// of orchestration only, so a script cannot recursively spawn scripts.
    pub(crate) fn subagent(mutation: bool) -> Self {
        Self {
            mutation,
            workflows: false,
        }
    }

    pub(crate) fn allow_mutation(self) -> bool {
        self.mutation
    }

    pub(crate) fn allow_workflows(self) -> bool {
        self.workflows
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ToolExecution {
    pub(crate) output: String,
    pub(crate) failed: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct TurnOutcome {
    pub(crate) event_count: usize,
    pub(crate) tool_calls: Vec<ToolCall>,
    /// Provider-native reasoning blocks that must round-trip unchanged across
    /// a tool continuation (notably OpenRouter reasoning_details).
    pub(crate) reasoning_details: Option<Vec<serde_json::Value>>,
    /// Usage for this single request, when the backend reported one.
    pub(crate) usage: Option<TokenUsage>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct PartialChatToolCall {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) arguments: String,
}
