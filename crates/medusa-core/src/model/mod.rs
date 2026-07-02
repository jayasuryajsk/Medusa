pub mod exec;
pub mod schema;
#[cfg(test)]
pub mod tests;
pub mod types;
pub mod wire;

use std::{path::PathBuf, time::Duration};

use color_eyre::eyre::{Result, WrapErr, bail};
use reqwest::{
    blocking::Client,
    header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue, USER_AGENT},
};
use serde_json::{Value, json};

use crate::auth::load_codex_oauth_credentials;
use crate::harness::HarnessPolicy;
use crate::tools::ToolRuntime;

pub use types::{
    ConversationAttachment, ConversationMessage, DirectCodexBackend, ModelStreamEvent,
};

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
        let compacted_messages =
            wire::compact_conversation_context(messages, wire::context_max_chars());
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

        if let Some(error) = tools
            .hooks()
            .run(HookEvent::turn_start(turn_mode, &latest_prompt))
            .blocking_failure_summary()
        {
            bail!("turn_start hook blocked turn: {error}");
        }

        loop {
            let outcome = self.stream_turn(
                input.clone(),
                &state,
                policy,
                tool_policy,
                skill_context.as_deref(),
                &mut on_event,
            )?;
            total_events += outcome.event_count;

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

            for call in outcome.tool_calls {
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

                let summary = exec::summarize_tool_call(&call);
                on_event(types::ModelStreamEvent::ToolStart {
                    name: exec::display_tool_name(&call.name).to_string(),
                    summary,
                })?;

                let execution =
                    exec::execute_tool_call_with_hooks(&tools, &call, &state, policy, tool_policy);
                on_event(types::ModelStreamEvent::ToolResult {
                    name: exec::display_tool_name(&call.name).to_string(),
                    output: exec::summarize_tool_result(&call, &execution),
                })?;

                exec::update_tool_loop_state(&mut state, &call, &execution);

                input.push(json!({
                    "type": "function_call_output",
                    "call_id": call.call_id,
                    "output": exec::compact_tool_context_output(&call, &execution),
                }));
            }
        }
    }

    fn stream_turn<F>(
        &self,
        input: Vec<Value>,
        state: &types::ToolLoopState,
        policy: HarnessPolicy,
        tool_policy: types::ToolLoopPolicy,
        skill_context: Option<&str>,
        on_event: &mut F,
    ) -> Result<types::TurnOutcome>
    where
        F: FnMut(types::ModelStreamEvent) -> Result<()>,
    {
        match self.provider {
            types::ModelProvider::Codex => {
                self.stream_codex_turn(input, state, policy, tool_policy, skill_context, on_event)
            }
            types::ModelProvider::DeepSeek | types::ModelProvider::OpenAiCompatible => self
                .stream_chat_completions_turn(
                    input,
                    state,
                    policy,
                    tool_policy,
                    skill_context,
                    on_event,
                ),
        }
    }

    fn stream_codex_turn<F>(
        &self,
        input: Vec<Value>,
        state: &types::ToolLoopState,
        policy: HarnessPolicy,
        tool_policy: types::ToolLoopPolicy,
        skill_context: Option<&str>,
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

        let allow_patch =
            policy.allows_patch() && tool_policy.allow_mutation() && !state.patch_requires_context;
        let mut body = json!({
            "model": self.model,
            "instructions": schema::medusa_instructions(&self.workspace, state, policy, skill_context),
            "input": input,
            "tools": schema::medusa_tools(allow_patch),
            "store": false,
            "stream": true,
        });

        if !self.reasoning_effort.eq_ignore_ascii_case("none") {
            body["reasoning"] = json!({
                "effort": self.reasoning_effort,
                "summary": "auto",
            });
        }

        let response = self
            .client
            .post("https://chatgpt.com/backend-api/codex/responses")
            .headers(headers)
            .json(&body)
            .send()
            .wrap_err("failed to call Codex backend")?;

        let status = response.status();
        if !status.is_success() {
            let text = response.text().wrap_err("failed to read Codex error")?;
            bail!(
                "Codex backend returned {status}: {}",
                exec::compact(&text, 360)
            );
        }

        wire::read_sse_response(response, on_event)
    }

    fn stream_chat_completions_turn<F>(
        &self,
        input: Vec<Value>,
        state: &types::ToolLoopState,
        policy: HarnessPolicy,
        tool_policy: types::ToolLoopPolicy,
        skill_context: Option<&str>,
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

        let allow_patch =
            policy.allows_patch() && tool_policy.allow_mutation() && !state.patch_requires_context;
        let mut body = json!({
            "model": self.model,
            "messages": wire::chat_completion_messages_from_input(
                input,
                &schema::medusa_instructions(&self.workspace, state, policy, skill_context),
            ),
            "tools": schema::chat_completion_tools(allow_patch),
            "tool_choice": "auto",
            "stream": true,
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

        let response = self
            .client
            .post(format!(
                "{}/chat/completions",
                self.chat_base_url.trim_end_matches('/')
            ))
            .headers(headers)
            .json(&body)
            .send()
            .wrap_err_with(|| format!("failed to call {} backend", self.provider.label()))?;

        let status = response.status();
        if !status.is_success() {
            let text = response
                .text()
                .wrap_err("failed to read chat backend error")?;
            bail!(
                "{} backend returned {status}: {}",
                self.provider.label(),
                exec::compact(&text, 360)
            );
        }

        wire::read_chat_completions_sse_response(response, on_event)
    }
}
