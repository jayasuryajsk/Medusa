pub mod exec;
pub mod provider;
pub mod schema;
#[cfg(test)]
pub mod tests;
pub mod types;
pub mod wire;

use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use color_eyre::eyre::{Result, WrapErr, bail};
use reqwest::{
    blocking::Client,
    header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, USER_AGENT},
};
use serde_json::{Value, json};

use crate::auth::load_codex_oauth_credentials;
use crate::cancel::CancelToken;
use crate::credentials::CredentialStore;
use crate::harness::HarnessPolicy;
use crate::tools::ToolRuntime;

pub(crate) fn retryable_status(status: u16) -> bool {
    status == 429 || (500..=504).contains(&status)
}

pub(crate) fn retry_backoff(attempt: u32, retry_after_seconds: Option<u64>) -> Duration {
    match retry_after_seconds {
        Some(seconds) => Duration::from_secs(seconds.clamp(1, 30)),
        None => Duration::from_millis(1_000 * 2u64.saturating_pow(attempt.saturating_sub(1))),
    }
}

fn retry_after_seconds(headers: &HeaderMap) -> Option<u64> {
    headers
        .get("retry-after")?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Sleep in ≤100ms slices, checking the cancel token between slices, so a
/// cancelled turn never waits out a full retry backoff (up to 30s).
pub(crate) fn sleep_with_cancel(duration: Duration, cancel: &CancelToken) -> Result<()> {
    let deadline = Instant::now() + duration;
    loop {
        cancel.bail_if_cancelled()?;
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return Ok(());
        };
        if remaining.is_zero() {
            return Ok(());
        }
        std::thread::sleep(remaining.min(Duration::from_millis(100)));
    }
}

const ULTRA_ORCHESTRATION_CONTEXT: &str = "\
Ultra orchestration mode is active. For substantial tasks with independent research, review, implementation, or verification lanes, proactively use workflow_run early to delegate useful work in parallel. Prefer parallel read-only agents for exploration and review, keep overlapping edits serialized, synthesize their evidence, and independently verify the integrated result. Do not delegate casual conversation, simple lookups, or one-step edits where coordination overhead would outweigh the benefit. The parent agent remains responsible for the final answer and verification.";

fn with_ultra_orchestration_context(
    base: Option<String>,
    reasoning_mode: &str,
    workflows_allowed: bool,
) -> Option<String> {
    if !reasoning_mode.eq_ignore_ascii_case("ultra") || !workflows_allowed {
        return base;
    }
    Some(match base {
        Some(base) if !base.trim().is_empty() => {
            format!("{base}\n\n{ULTRA_ORCHESTRATION_CONTEXT}")
        }
        _ => ULTRA_ORCHESTRATION_CONTEXT.to_string(),
    })
}

pub use types::{
    ConversationAttachment, ConversationMessage, DirectCodexBackend, ModelGateway,
    ModelStreamEvent, TokenUsage,
};

fn finish_tool_call<F>(
    on_event: &mut F,
    call: &types::ToolCall,
    execution: &types::ToolExecution,
) -> Result<()>
where
    F: FnMut(types::ModelStreamEvent) -> Result<()>,
{
    on_event(types::ModelStreamEvent::ToolResult {
        call_id: call.call_id.clone(),
        name: exec::display_tool_name(&call.name).to_string(),
        output: exec::summarize_tool_result(call, execution),
    })
}

fn runtime_context_item(content: &str) -> Value {
    json!({
        "role": "system",
        "content": content,
    })
}

fn sort_tool_schemas(tools: &mut [Value]) {
    tools.sort_by(|left, right| tool_schema_name(left).cmp(tool_schema_name(right)));
}

fn tool_schema_name(tool: &Value) -> &str {
    tool.get("name")
        .or_else(|| {
            tool.get("function")
                .and_then(|function| function.get("name"))
        })
        .and_then(Value::as_str)
        .unwrap_or("")
}

impl ModelGateway {
    pub fn new(workspace: impl Into<PathBuf>) -> Result<Self> {
        let workspace = workspace.into();
        let client = Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .wrap_err("failed to build HTTP client")?;
        let registry = provider::ProviderRegistry::load(&workspace)?;
        let env_model = std::env::var("MEDUSA_MODEL")
            .ok()
            .filter(|value| !value.trim().is_empty());
        let env_provider = std::env::var("MEDUSA_PROVIDER")
            .ok()
            .filter(|value| !value.trim().is_empty());
        let provider_hint = env_provider.as_deref().map(provider::canonical_provider_id);
        let selection = match env_model {
            Some(model) => registry.select(&model, env_provider.as_deref())?,
            None => registry.default_reference_for(env_provider.as_deref().unwrap_or("codex"))?,
        };
        let model_reference = selection.as_string();
        let reasoning_effort = std::env::var("MEDUSA_REASONING_EFFORT")
            .unwrap_or_else(|_| "medium".to_string())
            .trim()
            .to_string();

        Ok(Self {
            workspace,
            registry,
            selection,
            model_reference,
            provider_hint,
            reasoning_effort,
            credentials: CredentialStore::load(),
            client,
        })
    }

    pub fn model_name(&self) -> &str {
        &self.model_reference
    }

    pub fn model_reference(&self) -> &str {
        &self.model_reference
    }

    fn wire_model_name(&self) -> &str {
        self.selection.model()
    }

    pub fn provider_name(&self) -> &str {
        self.selection.provider()
    }

    pub fn model_capabilities(&self) -> provider::ModelCapabilities {
        self.provider_definition()
            .map(|provider| provider.model_capabilities(self.wire_model_name()))
            .unwrap_or_default()
    }

    pub fn context_window(&self) -> Option<usize> {
        self.provider_definition()
            .ok()
            .and_then(|provider| provider.models.get(self.wire_model_name()))
            .and_then(|model| model.context_window)
    }

    pub fn available_models(&self) -> Vec<crate::models::ModelInfo> {
        self.registry.model_catalog()
    }

    pub fn provider_status_lines(&self) -> Vec<String> {
        let Ok(provider) = self.provider_definition() else {
            return vec![format!(
                "provider: {} (not configured)",
                self.provider_name()
            )];
        };
        let protocol = match provider.protocol {
            provider::ProviderProtocol::CodexResponses => "Codex Responses",
            provider::ProviderProtocol::OpenAiResponses => "OpenAI Responses",
            provider::ProviderProtocol::OpenAiChat => "OpenAI Chat Completions",
        };
        let auth = match provider.auth {
            provider::ProviderAuth::CodexOauth => load_codex_oauth_credentials()
                .map(|_| "ready (Codex OAuth)".to_string())
                .unwrap_or_else(|error| format!("missing ({error})")),
            provider::ProviderAuth::Bearer => self
                .credentials
                .api_key(provider)
                .map(|_| "ready (API key)".to_string())
                .unwrap_or_else(|| format!("missing ({})", provider.auth_hint())),
            provider::ProviderAuth::None => "not required".to_string(),
        };
        vec![
            format!("provider: {} ({})", provider.display_name, provider.id),
            format!("model: {}", self.model_reference()),
            format!("protocol: {protocol}"),
            format!("endpoint: {}", provider.base_url),
            format!("auth: {auth}"),
        ]
    }

    pub fn set_model_name(&mut self, model: impl Into<String>) {
        let _ = self.try_set_model_name(model);
    }

    pub fn try_set_model_name(&mut self, model: impl Into<String>) -> Result<()> {
        let model = model.into();
        let normalized = model.trim().to_ascii_lowercase();
        let provider_hint = if model.contains('/') {
            None
        } else if let Some(provider) = self.provider_hint.clone() {
            Some(provider)
        } else if normalized.starts_with("gpt-") || normalized.starts_with("deepseek") {
            None
        } else {
            Some(self.provider_name().to_string())
        };
        let selection = self.registry.select(&model, provider_hint.as_deref())?;
        self.model_reference = selection.as_string();
        self.selection = selection;
        Ok(())
    }

    pub fn reasoning_effort(&self) -> &str {
        &self.reasoning_effort
    }

    pub fn set_reasoning_effort(&mut self, effort: impl Into<String>) {
        self.reasoning_effort = effort.into().trim().to_string();
    }

    fn provider_definition(&self) -> Result<&provider::ProviderDefinition> {
        self.registry.resolve(&self.selection)
    }

    #[cfg(test)]
    fn chat(&self, user_input: &str, tools: ToolRuntime) -> Result<types::ModelChatResult> {
        let mut response = String::new();
        let event_count = self.chat_stream(user_input, tools, |event| {
            if let types::ModelStreamEvent::Delta(delta) = event {
                response.push_str(&delta);
            }
            Ok(())
        })?;
        let response = response.trim().to_string();
        if response.is_empty() {
            bail!("model backend completed without output text");
        }

        Ok(types::ModelChatResult {
            response,
            event_count,
        })
    }

    #[cfg(test)]
    pub fn chat_stream<F>(&self, user_input: &str, tools: ToolRuntime, on_event: F) -> Result<usize>
    where
        F: FnMut(types::ModelStreamEvent) -> Result<()>,
    {
        self.chat_stream_messages(
            &[types::ConversationMessage {
                role: "user".to_string(),
                content: user_input.to_string(),
                attachments: Vec::new(),
            }],
            tools,
            on_event,
        )
    }

    pub fn chat_stream_messages<F>(
        &self,
        messages: &[types::ConversationMessage],
        tools: ToolRuntime,
        mut on_event: F,
    ) -> Result<usize>
    where
        F: FnMut(types::ModelStreamEvent) -> Result<()>,
    {
        self.chat_stream_messages_with_tool_policy(
            messages,
            tools,
            types::ToolLoopPolicy::mutation_allowed(),
            &mut on_event,
        )
    }

    pub fn chat_stream_messages_read_only<F>(
        &self,
        messages: &[types::ConversationMessage],
        tools: ToolRuntime,
        mut on_event: F,
    ) -> Result<usize>
    where
        F: FnMut(types::ModelStreamEvent) -> Result<()>,
    {
        self.chat_stream_messages_with_tool_policy(
            messages,
            tools,
            types::ToolLoopPolicy::read_only(),
            &mut on_event,
        )
    }

    pub(crate) fn chat_stream_messages_subagent<F>(
        &self,
        messages: &[types::ConversationMessage],
        tools: ToolRuntime,
        allow_mutation: bool,
        mut on_event: F,
    ) -> Result<usize>
    where
        F: FnMut(types::ModelStreamEvent) -> Result<()>,
    {
        self.chat_stream_messages_with_tool_policy(
            messages,
            tools,
            types::ToolLoopPolicy::subagent(allow_mutation),
            &mut on_event,
        )
    }

    fn chat_stream_messages_with_tool_policy<F>(
        &self,
        messages: &[types::ConversationMessage],
        tools: ToolRuntime,
        tool_policy: types::ToolLoopPolicy,
        mut on_event: F,
    ) -> Result<usize>
    where
        F: FnMut(types::ModelStreamEvent) -> Result<()>,
    {
        use crate::hooks::HookEvent;

        let latest_prompt = wire::latest_user_prompt(messages);
        let context_budget = crate::context::context_max_tokens();
        let compacted_messages = wire::compact_conversation_context(messages, context_budget);
        let mut input = compacted_messages
            .iter()
            .filter(|message| !message.content.trim().is_empty() || !message.attachments.is_empty())
            .map(wire::conversation_message_json)
            .collect::<Vec<_>>();
        let mut total_events = 0;
        let policy = HarnessPolicy::for_user_prompt_with_reasoning(
            &latest_prompt,
            self.reasoning_effort(),
            tool_policy.allow_workflows(),
        );
        let mut state = types::ToolLoopState::for_policy(policy);
        let mut no_progress = crate::orchestrator::NoProgressTracker::default();
        let turn_mode = policy.mode_label();
        let skill_context = tools.skills().prompt_context(&latest_prompt);
        let project_context = crate::project::project_instructions_context(&self.workspace);
        let extra_context = match (project_context, skill_context) {
            (Some(project), Some(skills)) => Some(format!("{project}\n\n{skills}")),
            (Some(project), None) => Some(project),
            (None, Some(skills)) => Some(skills),
            (None, None) => None,
        };
        let extra_context = with_ultra_orchestration_context(
            extra_context,
            self.reasoning_effort(),
            tool_policy.allow_workflows(),
        );

        if let Some(error) = tools
            .hooks()
            .run(HookEvent::turn_start(turn_mode, &latest_prompt))
            .blocking_failure_summary()
        {
            bail!("turn_start hook blocked turn: {error}");
        }

        // Dynamic MCP tool schemas, fetched once per turn on this worker
        // thread (first use lazily connects the servers, which can block).
        // Read-only turns only see servers the user marked "readOnly": true.
        let mut mcp_tools = tools.mcp_tool_schemas(tool_policy.allow_mutation());
        sort_tool_schemas(&mut mcp_tools);

        // Runtime context is input, not part of the stable instruction prefix.
        // Keep every emitted snapshot in the canonical input so each request
        // is an append-only extension of the previous one. This is the shape
        // provider prompt caches can reuse most effectively.
        let mut runtime_context =
            schema::medusa_runtime_context(&state, policy, extra_context.as_deref());
        input.push(runtime_context_item(&runtime_context));

        let cancel = tools.cancel_token().clone();
        let result = (|| -> Result<usize> {
            loop {
                cancel.bail_if_cancelled()?;

                let outcome = self.stream_turn(
                    input.clone(),
                    tool_policy,
                    &mcp_tools,
                    &cancel,
                    &mut on_event,
                )?;
                total_events += outcome.event_count;

                // One Usage event per request: a tool-looping turn makes many
                // requests, and consumers sum these for turn totals.
                if let Some(usage) = outcome.usage {
                    on_event(types::ModelStreamEvent::Usage(usage))?;
                }

                if outcome.tool_calls.is_empty() {
                    if let Some(feedback) = state.orchestrator.completion_feedback() {
                        input.push(json!({
                            "role": "developer",
                            "content": feedback,
                        }));
                        continue;
                    }
                    if let Some(error) = tools
                        .hooks()
                        .run(HookEvent::turn_end(turn_mode, "complete"))
                        .blocking_failure_summary()
                    {
                        bail!("turn_end hook failed: {error}");
                    }
                    return Ok(total_events);
                }

                let response_reasoning_details = outcome.reasoning_details;
                let calls = outcome.tool_calls;

                // Announce every call up front (in emission order) so the
                // transcript shows the whole batch before results stream in.
                for call in &calls {
                    let mut call_item = json!({
                        "type": "function_call",
                        "call_id": call.call_id,
                        "name": call.name,
                        "arguments": call.arguments,
                    });
                    if let Some(reasoning_content) = call.reasoning_content.as_deref()
                        && !reasoning_content.trim().is_empty()
                    {
                        call_item["reasoning_content"] = json!(reasoning_content);
                    }
                    if let Some(reasoning_details) = response_reasoning_details.as_ref()
                        && !reasoning_details.is_empty()
                    {
                        call_item["reasoning_details"] = json!(reasoning_details);
                    }
                    input.push(call_item);

                    on_event(types::ModelStreamEvent::ToolStart {
                        call_id: call.call_id.clone(),
                        name: exec::display_tool_name(&call.name).to_string(),
                        summary: exec::summarize_tool_call(call),
                    })?;
                }

                let executions = self.execute_turn_tool_calls(
                    &tools,
                    &calls,
                    &mut state,
                    policy,
                    tool_policy,
                    &mut on_event,
                )?;

                let attempts = calls
                    .iter()
                    .zip(executions.iter())
                    .map(|(call, execution)| crate::orchestrator::ToolAttempt {
                        name: &call.name,
                        arguments: &call.arguments,
                        output: &execution.output,
                        failed: execution.failed,
                        made_durable_progress: !execution.failed
                            && (exec::tool_call_is_file_mutation(&call.name)
                                || call.name == "workflow_run"),
                    })
                    .collect::<Vec<_>>();
                let progress = no_progress.observe(&attempts);

                // Model context outputs go back in emission order regardless of
                // completion order, so the conversation stays deterministic.
                for (call, execution) in calls.iter().zip(&executions) {
                    input.push(json!({
                        "type": "function_call_output",
                        "call_id": call.call_id,
                        "output": exec::compact_tool_context_output(call, execution),
                    }));
                }

                match progress {
                    crate::orchestrator::ProgressSignal::Progress => {}
                    crate::orchestrator::ProgressSignal::Warning(feedback) => {
                        input.push(json!({
                            "role": "developer",
                            "content": feedback,
                        }));
                    }
                    crate::orchestrator::ProgressSignal::Stalled(error) => {
                        bail!("{error}");
                    }
                }

                let updated_runtime_context =
                    schema::medusa_runtime_context(&state, policy, extra_context.as_deref());
                if updated_runtime_context != runtime_context {
                    input.push(runtime_context_item(&updated_runtime_context));
                    runtime_context = updated_runtime_context;
                }

                crate::context::prune_input_tool_outputs(&mut input, context_budget);
            }
        })();

        if let Err(error) = &result
            && crate::cancel::error_is_cancellation(error)
        {
            // Best-effort: the turn is unwinding on the user's Esc; a failing
            // hook must never mask or replace the cancellation itself.
            let _ = tools
                .hooks()
                .run(HookEvent::turn_end(turn_mode, "cancelled"));
        }
        result
    }

    /// Execute one turn's tool calls. Consecutive read-only calls fan out
    /// concurrently; anything mutating (or workflow_run) is a barrier and
    /// runs serially in emission order, preserving pre-parallel semantics.
    /// ToolResult events surface in completion order (call_id keys them in
    /// the UI); the returned executions are in emission order.
    fn execute_turn_tool_calls<F>(
        &self,
        tools: &ToolRuntime,
        calls: &[types::ToolCall],
        state: &mut types::ToolLoopState,
        policy: HarnessPolicy,
        tool_policy: types::ToolLoopPolicy,
        on_event: &mut F,
    ) -> Result<Vec<types::ToolExecution>>
    where
        F: FnMut(types::ModelStreamEvent) -> Result<()>,
    {
        let cancel = tools.cancel_token();
        let mut executions: Vec<Option<types::ToolExecution>> = vec![None; calls.len()];
        // Post-edit verification runs once per turn, after the batch's final
        // file mutation — mid-refactor intermediate states are expected to be
        // broken, so checking every edit would just produce false alarms.
        let last_mutation = calls
            .iter()
            .rposition(|call| exec::tool_call_is_file_mutation(&call.name));
        let mut turn_changed_files: Vec<String> = Vec::new();
        let mut index = 0;
        while index < calls.len() {
            // Cancellation is checked between calls, never mid-call, so a
            // mutation is never interrupted mid-write (files stay consistent).
            cancel.bail_if_cancelled()?;
            let call = &calls[index];

            if exec::tool_call_is_read_only(&call.name) {
                let mut end = index + 1;
                while end < calls.len() && exec::tool_call_is_read_only(&calls[end].name) {
                    end += 1;
                }

                if end - index == 1 {
                    let execution =
                        exec::execute_tool_call_with_hooks(tools, call, state, policy, tool_policy);
                    finish_tool_call(on_event, call, &execution)?;
                    executions[index] = Some(execution);
                } else {
                    let (sender, receiver) = std::sync::mpsc::channel();
                    for (offset, call) in calls[index..end].iter().enumerate() {
                        let sender = sender.clone();
                        let tools = tools.clone();
                        let call = call.clone();
                        let state = state.clone();
                        std::thread::spawn(move || {
                            let execution = exec::execute_tool_call_with_hooks(
                                &tools,
                                &call,
                                &state,
                                policy,
                                tool_policy,
                            );
                            let _ = sender.send((index + offset, execution));
                        });
                    }
                    drop(sender);
                    let mut remaining = end - index;
                    while remaining > 0 {
                        // Poll the token while collecting: on cancel, abandon
                        // the stragglers (read-only by classification; any
                        // explore terminal probes die via the shared token)
                        // after resolving their transcript rows.
                        if cancel.is_cancelled() {
                            for (offset, slot) in executions[index..end].iter_mut().enumerate() {
                                if slot.is_none() {
                                    let execution = types::ToolExecution {
                                        failed: true,
                                        output: "cancelled: interrupted by user".to_string(),
                                    };
                                    finish_tool_call(on_event, &calls[index + offset], &execution)?;
                                    *slot = Some(execution);
                                }
                            }
                            cancel.bail_if_cancelled()?;
                        }
                        match receiver.recv_timeout(Duration::from_millis(100)) {
                            Ok((slot, execution)) => {
                                finish_tool_call(on_event, &calls[slot], &execution)?;
                                executions[slot] = Some(execution);
                                remaining -= 1;
                            }
                            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                        }
                    }
                }

                // Read-only calls never consult ToolLoopState during
                // execution, so applying updates at batch end in emission
                // order preserves serial semantics.
                for (offset, execution) in executions[index..end].iter().enumerate() {
                    if let Some(execution) = execution {
                        exec::update_tool_loop_state(state, &calls[index + offset], execution);
                    }
                }
                index = end;
                continue;
            }

            let mut execution = if call.name == "workflow_run" {
                exec::execute_workflow_run_with_hooks(
                    tools,
                    call,
                    policy,
                    tool_policy,
                    self,
                    on_event,
                )
            } else {
                exec::execute_tool_call_with_hooks(tools, call, state, policy, tool_policy)
            };
            if exec::tool_call_is_file_mutation(&call.name) && !execution.failed {
                turn_changed_files.extend(exec::mutation_changed_files(&execution.output));
            }
            if Some(index) == last_mutation
                && !execution.failed
                && let Some(verdict) = crate::verify::verify_after_mutation(
                    tools.workspace(),
                    &turn_changed_files,
                    cancel,
                )
            {
                execution.output.push('\n');
                execution.output.push_str(&verdict);
            }
            finish_tool_call(on_event, call, &execution)?;
            exec::update_tool_loop_state(state, call, &execution);
            executions[index] = Some(execution);
            index += 1;
        }

        Ok(executions
            .into_iter()
            .map(|execution| {
                execution.unwrap_or_else(|| types::ToolExecution {
                    failed: true,
                    output: "error: tool worker ended without returning a result".to_string(),
                })
            })
            .collect())
    }

    /// One-shot, tool-free completion used for internal plumbing such as
    /// context compaction summaries. Returns the model's text output. Takes
    /// the turn's cancel token so Esc during compaction doesn't hang until
    /// the summary finishes.
    pub fn plain_completion(
        &self,
        instructions: &str,
        user_text: &str,
        cancel: &CancelToken,
    ) -> Result<String> {
        let user_message = types::ConversationMessage {
            role: "user".to_string(),
            content: user_text.to_string(),
            attachments: Vec::new(),
        };
        let input = vec![wire::conversation_message_json(&user_message)];

        let mut text = String::new();
        let mut on_event = |event: types::ModelStreamEvent| {
            if let types::ModelStreamEvent::Delta(delta) = event {
                text.push_str(&delta);
            }
            Ok(())
        };

        let provider = self.provider_definition()?.clone();
        let headers = self.provider_headers(&provider)?;
        match provider.protocol {
            provider::ProviderProtocol::CodexResponses
            | provider::ProviderProtocol::OpenAiResponses => {
                let mut body = json!({
                    "model": self.wire_model_name(),
                    "instructions": instructions,
                    "input": input,
                    "tools": [],
                    "store": false,
                    "stream": true,
                });
                if provider
                    .model_capabilities(self.wire_model_name())
                    .reasoning
                {
                    body["reasoning"] = json!({ "effort": "low" });
                }
                let url = provider.endpoint("responses");
                let response = self.send_model_request(
                    || self.client.post(&url).headers(headers.clone()).json(&body),
                    &provider.display_name,
                    cancel,
                )?;
                wire::read_sse_response(response, cancel, &mut on_event)?;
            }
            provider::ProviderProtocol::OpenAiChat => {
                let capabilities = provider.model_capabilities(self.wire_model_name());
                let mut body = json!({
                    "model": self.wire_model_name(),
                    "messages": wire::chat_completion_messages_from_input_with_images(
                        input,
                        instructions,
                        capabilities.images,
                    ),
                    "stream": true,
                    "stream_options": { "include_usage": true },
                });
                if capabilities.reasoning {
                    schema::apply_chat_reasoning(&mut body, provider.thinking, "low");
                }
                let url = provider.endpoint("chat/completions");
                let response = self.send_model_request(
                    || self.client.post(&url).headers(headers.clone()).json(&body),
                    &provider.display_name,
                    cancel,
                )?;
                wire::read_chat_completions_sse_response(response, cancel, &mut on_event)?;
            }
        }

        Ok(text.trim().to_string())
    }

    /// Send the initial model request, retrying transient failures (transport
    /// errors, 429, 5xx) with backoff. Only the pre-stream request is retried;
    /// once SSE bytes start flowing a failure surfaces to the caller. Backoff
    /// sleeps are sliced so cancellation interrupts them within ~100ms.
    fn send_model_request(
        &self,
        build: impl Fn() -> reqwest::blocking::RequestBuilder,
        label: &str,
        cancel: &CancelToken,
    ) -> Result<reqwest::blocking::Response> {
        const MAX_ATTEMPTS: u32 = 3;

        let mut attempt = 0;
        loop {
            cancel.bail_if_cancelled()?;
            attempt += 1;
            match build().send() {
                Ok(response) => {
                    let status = response.status();
                    if status.is_success() {
                        return Ok(response);
                    }
                    let retry_after = retry_after_seconds(response.headers());
                    let text = response.text().unwrap_or_default();
                    if attempt < MAX_ATTEMPTS && retryable_status(status.as_u16()) {
                        sleep_with_cancel(retry_backoff(attempt, retry_after), cancel)?;
                        continue;
                    }
                    bail!(
                        "{label} returned {status} after {attempt} attempt{}: {}",
                        if attempt == 1 { "" } else { "s" },
                        exec::compact(&text, 360)
                    );
                }
                Err(error) if attempt < MAX_ATTEMPTS => {
                    sleep_with_cancel(retry_backoff(attempt, None), cancel)?;
                    let _ = error;
                }
                Err(error) => {
                    return Err(error).wrap_err_with(|| {
                        format!("failed to call {label} after {attempt} attempts")
                    });
                }
            }
        }
    }

    fn provider_headers(&self, provider: &provider::ProviderDefinition) -> Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
        headers.insert(
            USER_AGENT,
            HeaderValue::from_static(concat!("medusa-tui/", env!("CARGO_PKG_VERSION"))),
        );

        match provider.auth {
            provider::ProviderAuth::CodexOauth => {
                let credentials = load_codex_oauth_credentials()?;
                headers.insert(
                    AUTHORIZATION,
                    HeaderValue::from_str(&format!("Bearer {}", credentials.bearer_token()))
                        .wrap_err("failed to build auth header")?,
                );
                if let Some(account_id) = credentials.account_id() {
                    headers.insert(
                        "ChatGPT-Account-ID",
                        HeaderValue::from_str(account_id)
                            .wrap_err("failed to build account header")?,
                    );
                }
            }
            provider::ProviderAuth::Bearer => {
                let api_key = self.credentials.api_key(provider).ok_or_else(|| {
                    color_eyre::eyre::eyre!(
                        "{} provider requires {}",
                        provider.display_name,
                        provider.auth_hint()
                    )
                })?;
                headers.insert(
                    AUTHORIZATION,
                    HeaderValue::from_str(&format!("Bearer {api_key}"))
                        .wrap_err("failed to build auth header")?,
                );
            }
            provider::ProviderAuth::None => {}
        }

        for (name, value) in &provider.headers {
            headers.insert(
                HeaderName::from_bytes(name.as_bytes())
                    .wrap_err_with(|| format!("invalid header name `{name}`"))?,
                HeaderValue::from_str(value)
                    .wrap_err_with(|| format!("invalid value for provider header `{name}`"))?,
            );
        }
        Ok(headers)
    }

    #[allow(clippy::too_many_arguments)]
    fn stream_turn<F>(
        &self,
        input: Vec<Value>,
        tool_policy: types::ToolLoopPolicy,
        extra_tools: &[Value],
        cancel: &CancelToken,
        on_event: &mut F,
    ) -> Result<types::TurnOutcome>
    where
        F: FnMut(types::ModelStreamEvent) -> Result<()>,
    {
        match self.provider_definition()?.protocol {
            provider::ProviderProtocol::CodexResponses
            | provider::ProviderProtocol::OpenAiResponses => {
                self.stream_responses_turn(input, tool_policy, extra_tools, cancel, on_event)
            }
            provider::ProviderProtocol::OpenAiChat => {
                self.stream_chat_completions_turn(input, tool_policy, extra_tools, cancel, on_event)
            }
        }
    }

    /// Named agents advertised in the workflow_run tool description so the
    /// model can reference them via agentType. Reloaded per turn so edits to
    /// .medusa/agents take effect without a restart.
    fn workflow_agent_names(&self, tool_policy: types::ToolLoopPolicy) -> Vec<String> {
        if !tool_policy.allow_workflows() {
            return Vec::new();
        }
        crate::agents::AgentRegistry::load(&self.workspace)
            .map(|registry| registry.names())
            .unwrap_or_default()
    }

    #[allow(clippy::too_many_arguments)]
    fn stream_responses_turn<F>(
        &self,
        input: Vec<Value>,
        tool_policy: types::ToolLoopPolicy,
        extra_tools: &[Value],
        cancel: &CancelToken,
        on_event: &mut F,
    ) -> Result<types::TurnOutcome>
    where
        F: FnMut(types::ModelStreamEvent) -> Result<()>,
    {
        let provider = self.provider_definition()?.clone();
        let headers = self.provider_headers(&provider)?;

        // Tool availability is stable for the whole turn. Orchestration gates
        // are enforced by the executor, avoiding schema churn after reads.
        let capabilities = provider.model_capabilities(self.wire_model_name());
        let allow_patch = tool_policy.allow_mutation();
        let mut tool_schemas = if capabilities.tools {
            schema::medusa_tools(
                allow_patch,
                tool_policy.allow_workflows(),
                &self.workflow_agent_names(tool_policy),
            )
        } else {
            Vec::new()
        };
        if capabilities.tools {
            tool_schemas.extend_from_slice(extra_tools);
        }
        let mut body = json!({
            "model": self.wire_model_name(),
            "instructions": schema::medusa_instructions(
                &self.workspace,
                !extra_tools.is_empty(),
            ),
            "input": input,
            "tools": tool_schemas,
            "store": false,
            "stream": true,
        });
        if capabilities.tools {
            body["parallel_tool_calls"] = json!(capabilities.parallel_tools);
        }

        let wire_effort =
            schema::responses_reasoning_effort(provider.thinking, &self.reasoning_effort);
        if capabilities.reasoning && !wire_effort.eq_ignore_ascii_case("none") {
            body["reasoning"] = json!({
                "effort": wire_effort,
                "summary": "auto",
            });
        }

        let url = provider.endpoint("responses");
        let response = self.send_model_request(
            || self.client.post(&url).headers(headers.clone()).json(&body),
            &provider.display_name,
            cancel,
        )?;

        wire::read_sse_response(response, cancel, on_event)
    }

    #[allow(clippy::too_many_arguments)]
    fn stream_chat_completions_turn<F>(
        &self,
        input: Vec<Value>,
        tool_policy: types::ToolLoopPolicy,
        extra_tools: &[Value],
        cancel: &CancelToken,
        on_event: &mut F,
    ) -> Result<types::TurnOutcome>
    where
        F: FnMut(types::ModelStreamEvent) -> Result<()>,
    {
        let provider = self.provider_definition()?.clone();
        let headers = self.provider_headers(&provider)?;

        // Keep schemas stable inside a turn; runtime enforcement remains the
        // source of truth for exploration and recovery gates.
        let capabilities = provider.model_capabilities(self.wire_model_name());
        let allow_patch = tool_policy.allow_mutation();
        let mut body = json!({
            "model": self.wire_model_name(),
            "messages": wire::chat_completion_messages_from_input_with_images(
                input,
                &schema::medusa_instructions(
                    &self.workspace,
                    !extra_tools.is_empty(),
                ),
                capabilities.images,
            ),
            "stream": true,
            // Ask for the final usage chunk (OpenAI omits it by default).
            "stream_options": { "include_usage": true },
        });
        if capabilities.tools {
            body["tools"] = json!(schema::chat_completion_tools(
                allow_patch,
                tool_policy.allow_workflows(),
                &self.workflow_agent_names(tool_policy),
                extra_tools,
            ));
            body["tool_choice"] = json!("auto");
            body["parallel_tool_calls"] = json!(capabilities.parallel_tools);
        }

        if capabilities.reasoning
            && !std::env::var("MEDUSA_THINKING")
                .map(|value| value.eq_ignore_ascii_case("disabled"))
                .unwrap_or(false)
        {
            schema::apply_chat_reasoning(&mut body, provider.thinking, &self.reasoning_effort);
        }

        let url = provider.endpoint("chat/completions");
        let response = self.send_model_request(
            || self.client.post(&url).headers(headers.clone()).json(&body),
            &provider.display_name,
            cancel,
        )?;

        wire::read_chat_completions_sse_response(response, cancel, on_event)
    }
}
