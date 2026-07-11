pub mod exec;
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
    header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue, USER_AGENT},
};
use serde_json::{Value, json};

use crate::auth::load_codex_oauth_credentials;
use crate::cancel::CancelToken;
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

pub use types::{
    ConversationAttachment, ConversationMessage, DirectCodexBackend, ModelStreamEvent, TokenUsage,
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

impl DirectCodexBackend {
    pub fn new(workspace: impl Into<PathBuf>) -> Result<Self> {
        use types::ModelProvider;

        let client = Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .wrap_err("failed to build HTTP client")?;
        let env_model = std::env::var("MEDUSA_MODEL").ok();
        let env_provider = std::env::var("MEDUSA_PROVIDER")
            .ok()
            .and_then(|value| ModelProvider::from_name(&value));
        let provider_locked = env_provider.is_some();
        let provider = env_provider
            .or_else(|| {
                env_model
                    .as_deref()
                    .and_then(ModelProvider::infer_from_model)
            })
            .unwrap_or(ModelProvider::Codex);
        let model = env_model.unwrap_or_else(|| provider.default_model().to_string());
        let reasoning_effort = std::env::var("MEDUSA_REASONING_EFFORT")
            .unwrap_or_else(|_| "medium".to_string())
            .trim()
            .to_string();
        let chat_base_url = provider.base_url();
        let chat_api_key = provider.api_key();

        Ok(Self {
            workspace: workspace.into(),
            provider,
            provider_locked,
            model,
            reasoning_effort,
            chat_base_url,
            chat_api_key,
            client,
        })
    }

    pub fn model_name(&self) -> &str {
        &self.model
    }

    pub fn provider_name(&self) -> &'static str {
        self.provider.label()
    }

    pub fn set_model_name(&mut self, model: impl Into<String>) {
        self.model = model.into();
        if !self.provider_locked
            && let Some(provider) = types::ModelProvider::infer_from_model(&self.model)
        {
            self.set_provider(provider);
        }
    }

    pub fn reasoning_effort(&self) -> &str {
        &self.reasoning_effort
    }

    pub fn set_reasoning_effort(&mut self, effort: impl Into<String>) {
        self.reasoning_effort = effort.into().trim().to_string();
    }

    fn set_provider(&mut self, provider: types::ModelProvider) {
        self.provider = provider;
        self.chat_base_url = provider.base_url();
        self.chat_api_key = provider.api_key();
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
            bail!("Codex backend completed without output text");
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
        let mut state = types::ToolLoopState::default();
        let policy = HarnessPolicy::for_user_prompt(&latest_prompt);
        let turn_mode = policy.mode_label();
        let skill_context = tools.skills().prompt_context(&latest_prompt);
        let project_context = crate::project::project_instructions_context(&self.workspace);
        let extra_context = match (project_context, skill_context) {
            (Some(project), Some(skills)) => Some(format!("{project}\n\n{skills}")),
            (Some(project), None) => Some(project),
            (None, Some(skills)) => Some(skills),
            (None, None) => None,
        };

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
        let mcp_tools = tools.mcp_tool_schemas(tool_policy.allow_mutation());

        let cancel = tools.cancel_token().clone();
        let result = (|| -> Result<usize> {
            loop {
                cancel.bail_if_cancelled()?;

                let outcome = self.stream_turn(
                    input.clone(),
                    &state,
                    policy,
                    tool_policy,
                    extra_context.as_deref(),
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
                    if let Some(error) = tools
                        .hooks()
                        .run(HookEvent::turn_end(turn_mode, "complete"))
                        .blocking_failure_summary()
                    {
                        bail!("turn_end hook failed: {error}");
                    }
                    return Ok(total_events);
                }

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

                // Model context outputs go back in emission order regardless of
                // completion order, so the conversation stays deterministic.
                for (call, execution) in calls.iter().zip(executions) {
                    input.push(json!({
                        "type": "function_call_output",
                        "call_id": call.call_id,
                        "output": exec::compact_tool_context_output(call, &execution),
                    }));
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

        match self.provider {
            types::ModelProvider::Codex => {
                let credentials = load_codex_oauth_credentials()?;
                let mut headers = HeaderMap::new();
                headers.insert(
                    AUTHORIZATION,
                    HeaderValue::from_str(&format!("Bearer {}", credentials.bearer_token()))
                        .wrap_err("failed to build auth header")?,
                );
                headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
                headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
                headers.insert(USER_AGENT, HeaderValue::from_static("medusa-tui/0.1.0"));
                if let Some(account_id) = credentials.account_id() {
                    headers.insert(
                        "ChatGPT-Account-ID",
                        HeaderValue::from_str(account_id)
                            .wrap_err("failed to build account header")?,
                    );
                }

                let body = json!({
                    "model": self.model,
                    "instructions": instructions,
                    "input": input,
                    "tools": [],
                    "store": false,
                    "stream": true,
                    "reasoning": { "effort": "low" },
                });
                let response = self.send_model_request(
                    || {
                        self.client
                            .post("https://chatgpt.com/backend-api/codex/responses")
                            .headers(headers.clone())
                            .json(&body)
                    },
                    "Codex backend",
                    cancel,
                )?;
                wire::read_sse_response(response, cancel, &mut on_event)?;
            }
            types::ModelProvider::DeepSeek | types::ModelProvider::OpenAiCompatible => {
                let Some(api_key) = self.chat_api_key.as_ref() else {
                    bail!(
                        "{} backend requires {}",
                        self.provider.label(),
                        self.provider.auth_hint()
                    );
                };
                let mut headers = HeaderMap::new();
                headers.insert(
                    AUTHORIZATION,
                    HeaderValue::from_str(&format!("Bearer {api_key}"))
                        .wrap_err("failed to build auth header")?,
                );
                headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
                headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
                headers.insert(USER_AGENT, HeaderValue::from_static("medusa-tui/0.1.0"));

                let body = json!({
                    "model": self.model,
                    "messages": wire::chat_completion_messages_from_input(input, instructions),
                    "stream": true,
                });
                let url = format!(
                    "{}/chat/completions",
                    self.chat_base_url.trim_end_matches('/')
                );
                let response = self.send_model_request(
                    || self.client.post(&url).headers(headers.clone()).json(&body),
                    self.provider.label(),
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

    #[allow(clippy::too_many_arguments)]
    fn stream_turn<F>(
        &self,
        input: Vec<Value>,
        state: &types::ToolLoopState,
        policy: HarnessPolicy,
        tool_policy: types::ToolLoopPolicy,
        extra_context: Option<&str>,
        extra_tools: &[Value],
        cancel: &CancelToken,
        on_event: &mut F,
    ) -> Result<types::TurnOutcome>
    where
        F: FnMut(types::ModelStreamEvent) -> Result<()>,
    {
        match self.provider {
            types::ModelProvider::Codex => self.stream_codex_turn(
                input,
                state,
                policy,
                tool_policy,
                extra_context,
                extra_tools,
                cancel,
                on_event,
            ),
            types::ModelProvider::DeepSeek | types::ModelProvider::OpenAiCompatible => self
                .stream_chat_completions_turn(
                    input,
                    state,
                    policy,
                    tool_policy,
                    extra_context,
                    extra_tools,
                    cancel,
                    on_event,
                ),
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
    fn stream_codex_turn<F>(
        &self,
        input: Vec<Value>,
        state: &types::ToolLoopState,
        policy: HarnessPolicy,
        tool_policy: types::ToolLoopPolicy,
        extra_context: Option<&str>,
        extra_tools: &[Value],
        cancel: &CancelToken,
        on_event: &mut F,
    ) -> Result<types::TurnOutcome>
    where
        F: FnMut(types::ModelStreamEvent) -> Result<()>,
    {
        let credentials = load_codex_oauth_credentials()?;
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", credentials.bearer_token()))
                .wrap_err("failed to build auth header")?,
        );
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
        headers.insert(USER_AGENT, HeaderValue::from_static("medusa-tui/0.1.0"));
        if let Some(account_id) = credentials.account_id() {
            headers.insert(
                "ChatGPT-Account-ID",
                HeaderValue::from_str(account_id).wrap_err("failed to build account header")?,
            );
        }

        let allow_patch = tool_policy.allow_mutation() && !state.patch_requires_context;
        let mut tool_schemas = schema::medusa_tools(
            allow_patch,
            tool_policy.allow_workflows(),
            &self.workflow_agent_names(tool_policy),
        );
        tool_schemas.extend_from_slice(extra_tools);
        let mut body = json!({
            "model": self.model,
            "instructions": schema::medusa_instructions(
                &self.workspace,
                state,
                policy,
                extra_context,
                !extra_tools.is_empty(),
            ),
            "input": input,
            "tools": tool_schemas,
            "store": false,
            "stream": true,
        });

        if !self.reasoning_effort.eq_ignore_ascii_case("none") {
            body["reasoning"] = json!({
                "effort": self.reasoning_effort,
                "summary": "auto",
            });
        }

        let response = self.send_model_request(
            || {
                self.client
                    .post("https://chatgpt.com/backend-api/codex/responses")
                    .headers(headers.clone())
                    .json(&body)
            },
            "Codex backend",
            cancel,
        )?;

        wire::read_sse_response(response, cancel, on_event)
    }

    #[allow(clippy::too_many_arguments)]
    fn stream_chat_completions_turn<F>(
        &self,
        input: Vec<Value>,
        state: &types::ToolLoopState,
        policy: HarnessPolicy,
        tool_policy: types::ToolLoopPolicy,
        extra_context: Option<&str>,
        extra_tools: &[Value],
        cancel: &CancelToken,
        on_event: &mut F,
    ) -> Result<types::TurnOutcome>
    where
        F: FnMut(types::ModelStreamEvent) -> Result<()>,
    {
        let Some(api_key) = self.chat_api_key.as_ref() else {
            bail!(
                "{} backend requires {}",
                self.provider.label(),
                self.provider.auth_hint()
            );
        };

        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {api_key}"))
                .wrap_err("failed to build auth header")?,
        );
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
        headers.insert(USER_AGENT, HeaderValue::from_static("medusa-tui/0.1.0"));

        let allow_patch = tool_policy.allow_mutation() && !state.patch_requires_context;
        let mut body = json!({
            "model": self.model,
            "messages": wire::chat_completion_messages_from_input(
                input,
                &schema::medusa_instructions(
                    &self.workspace,
                    state,
                    policy,
                    extra_context,
                    !extra_tools.is_empty(),
                ),
            ),
            "tools": schema::chat_completion_tools(
                allow_patch,
                tool_policy.allow_workflows(),
                &self.workflow_agent_names(tool_policy),
                extra_tools,
            ),
            "tool_choice": "auto",
            "stream": true,
            // Ask for the final usage chunk (OpenAI omits it by default).
            "stream_options": { "include_usage": true },
        });

        if self.provider == types::ModelProvider::DeepSeek
            && !self.reasoning_effort.eq_ignore_ascii_case("none")
            && !std::env::var("MEDUSA_THINKING")
                .map(|value| value.eq_ignore_ascii_case("disabled"))
                .unwrap_or(false)
        {
            body["thinking"] = json!({
                "type": "enabled",
            });
            body["reasoning_effort"] =
                json!(schema::deepseek_reasoning_effort(&self.reasoning_effort));
        }

        let url = format!(
            "{}/chat/completions",
            self.chat_base_url.trim_end_matches('/')
        );
        let response = self.send_model_request(
            || self.client.post(&url).headers(headers.clone()).json(&body),
            self.provider.label(),
            cancel,
        )?;

        wire::read_chat_completions_sse_response(response, cancel, on_event)
    }
}
