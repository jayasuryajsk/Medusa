use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader, Read},
    sync::mpsc::{Receiver, RecvTimeoutError, sync_channel},
    thread,
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use color_eyre::eyre::{Result, WrapErr, bail};
use serde_json::{Value, json};

use crate::cancel::CancelToken;
use crate::model::types::*;

pub(crate) fn conversation_message_json(message: &ConversationMessage) -> Value {
    if message.attachments.is_empty() {
        return json!({ "role": message.role, "content": message.content });
    }

    let mut content = Vec::new();
    if !message.content.trim().is_empty() {
        content.push(json!({ "type": "input_text", "text": message.content }));
    }

    for attachment in &message.attachments {
        match fs::read(&attachment.path) {
            Ok(bytes) => content.push(json!({
                "type": "input_image",
                "image_url": format!(
                    "data:{};base64,{}",
                    attachment.mime,
                    BASE64.encode(bytes)
                ),
            })),
            Err(error) => content.push(json!({
                "type": "input_text",
                "text": format!("[Medusa could not load image {}: {error}]", attachment.path.display()),
            })),
        }
    }

    json!({ "role": message.role, "content": content })
}

pub(crate) fn chat_completion_messages_from_input(
    input: Vec<Value>,
    instructions: &str,
) -> Vec<Value> {
    let mut messages = vec![json!({
        "role": "system",
        "content": instructions,
    })];

    for item in input {
        if let Some(role) = item.get("role").and_then(Value::as_str) {
            let content = chat_completion_content_text(item.get("content").unwrap_or(&Value::Null));
            if !content.trim().is_empty() {
                messages.push(json!({
                    "role": role,
                    "content": content,
                }));
            }
            continue;
        }

        match item.get("type").and_then(Value::as_str) {
            Some("function_call") => {
                let Some(call_id) = item.get("call_id").and_then(Value::as_str) else {
                    continue;
                };
                let Some(name) = item.get("name").and_then(Value::as_str) else {
                    continue;
                };
                let arguments = item
                    .get("arguments")
                    .and_then(Value::as_str)
                    .unwrap_or("{}");
                let mut message = json!({
                    "role": "assistant",
                    "content": Value::Null,
                    "tool_calls": [{
                        "id": call_id,
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": arguments,
                        }
                    }],
                });
                if let Some(reasoning_content) =
                    item.get("reasoning_content").and_then(Value::as_str)
                    && !reasoning_content.trim().is_empty()
                {
                    message["reasoning_content"] = json!(reasoning_content);
                }
                messages.push(message);
            }
            Some("function_call_output") => {
                let Some(call_id) = item.get("call_id").and_then(Value::as_str) else {
                    continue;
                };
                let output = item.get("output").and_then(Value::as_str).unwrap_or("");
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": call_id,
                    "content": output,
                }));
            }
            _ => {}
        }
    }

    messages
}

pub(crate) fn chat_completion_content_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.to_string(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| {
                if let Some(text) = part
                    .get("text")
                    .or_else(|| part.get("content"))
                    .and_then(Value::as_str)
                {
                    return Some(text.to_string());
                }

                if part
                    .get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|kind| kind.contains("image"))
                {
                    return Some("[image attachment omitted: this chat backend does not support Medusa image input yet]".to_string());
                }

                None
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

pub(crate) fn latest_user_prompt(messages: &[ConversationMessage]) -> String {
    messages
        .iter()
        .rev()
        .find(|message| message.role == "user")
        .map(|message| message.content.clone())
        .unwrap_or_default()
}

pub(crate) fn compact_conversation_context(
    messages: &[ConversationMessage],
    max_tokens: usize,
) -> Vec<ConversationMessage> {
    let total_tokens = messages
        .iter()
        .map(crate::context::message_tokens)
        .sum::<usize>();
    if total_tokens <= max_tokens || messages.len() <= 2 {
        return messages.to_vec();
    }

    let system_prefix_len = messages
        .iter()
        .take_while(|message| message.role == "system")
        .count();
    let (system_prefix, body) = messages.split_at(system_prefix_len);
    if body.is_empty() {
        return messages.to_vec();
    }

    let reserved_tokens = system_prefix
        .iter()
        .map(crate::context::message_tokens)
        .sum::<usize>();
    let body_budget = max_tokens.saturating_sub(reserved_tokens).max(1);

    let mut kept = Vec::new();
    let mut used = 0usize;
    for message in body.iter().rev() {
        let cost = crate::context::message_tokens(message).max(1);
        if !kept.is_empty() && used + cost > body_budget {
            break;
        }
        used += cost;
        kept.push(message.clone());
    }
    kept.reverse();

    let omitted = body.len().saturating_sub(kept.len());
    if omitted == 0 {
        return messages.to_vec();
    }

    let mut compacted = Vec::with_capacity(system_prefix.len() + kept.len() + 1);
    compacted.extend(system_prefix.iter().cloned());
    compacted.push(ConversationMessage {
        role: "system".to_string(),
        content: format!(
            "Medusa context compaction omitted {omitted} older transcript messages. Continue from the visible recent context; inspect files with file_read, file_search, or fs_list when exact state matters."
        ),
        attachments: Vec::new(),
    });
    compacted.extend(kept);
    compacted
}

/// The blocking SSE read can't be interrupted in place, so a pump thread
/// owns the body and forwards lines over a bounded channel; the parse loop
/// polls the cancel token while the stream is quiet. After cancellation the
/// pump dies on its next send failure (bounded by the HTTP client timeout).
fn spawn_sse_line_pump<R>(source: R) -> Receiver<std::io::Result<String>>
where
    R: Read + Send + 'static,
{
    let (sender, receiver) = sync_channel(64);
    thread::spawn(move || {
        for line in BufReader::new(source).lines() {
            let failed = line.is_err();
            if sender.send(line).is_err() || failed {
                return;
            }
        }
    });
    receiver
}

/// Next line from the pump, or None at end of stream. Timeouts poll the
/// cancel token, so Esc interrupts a stalled read within ~100ms.
fn next_pumped_line(
    lines: &Receiver<std::io::Result<String>>,
    cancel: &CancelToken,
    context: &'static str,
) -> Result<Option<String>> {
    loop {
        match lines.recv_timeout(Duration::from_millis(100)) {
            Ok(Ok(line)) => return Ok(Some(line)),
            Ok(Err(error)) => return Err(error).wrap_err(context),
            Err(RecvTimeoutError::Timeout) => cancel.bail_if_cancelled()?,
            // Pump thread finished and hung up: EOF.
            Err(RecvTimeoutError::Disconnected) => return Ok(None),
        }
    }
}

pub(crate) fn read_sse_response<R, F>(
    response: R,
    cancel: &CancelToken,
    on_event: &mut F,
) -> Result<TurnOutcome>
where
    R: Read + Send + 'static,
    F: FnMut(ModelStreamEvent) -> Result<()>,
{
    let lines = spawn_sse_line_pump(response);
    read_sse_lines(
        &mut || next_pumped_line(&lines, cancel, "failed to read SSE line"),
        on_event,
    )
}

fn read_sse_lines<F>(
    next_line: &mut dyn FnMut() -> Result<Option<String>>,
    on_event: &mut F,
) -> Result<TurnOutcome>
where
    F: FnMut(ModelStreamEvent) -> Result<()>,
{
    let mut emitted_text = false;
    let mut completed_text = String::new();
    let mut event_count = 0;
    let mut tool_calls = Vec::new();
    let mut usage = None;

    while let Some(line) = next_line()? {
        let Some(payload) = line.strip_prefix("data: ") else {
            continue;
        };

        if payload == "[DONE]" {
            break;
        }

        event_count += 1;
        let event = parse_sse_payload(payload)?;
        match event.get("type").and_then(Value::as_str) {
            Some("response.output_text.delta") | Some("response.refusal.delta") => {
                if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                    emitted_text = true;
                    on_event(ModelStreamEvent::Delta(delta.to_string()))?;
                }
            }
            Some("response.output_text.done") => {
                if let Some(done_text) = event.get("text").and_then(Value::as_str) {
                    completed_text = done_text.to_string();
                }
            }
            Some("response.output_item.done") => {
                if let Some(call) = extract_tool_call(&event) {
                    tool_calls.push(call);
                }
            }
            Some("response.completed") => {
                usage = event
                    .get("response")
                    .and_then(|response| response.get("usage"))
                    .or_else(|| event.get("usage"))
                    .and_then(parse_token_usage);

                for call in extract_completed_tool_calls(&event) {
                    if !tool_calls
                        .iter()
                        .any(|existing| existing.call_id == call.call_id)
                    {
                        tool_calls.push(call);
                    }
                }

                for text in extract_reasoning_text(&event) {
                    on_event(ModelStreamEvent::ReasoningDelta(text))?;
                }

                if !emitted_text && let Some(text) = extract_completed_output_text(&event) {
                    completed_text = text;
                }
                break;
            }
            Some("response.failed") | Some("response.incomplete") => {
                bail!("{}", backend_failure_message(&event));
            }
            Some(event_type) if event_type.contains("reasoning") => {
                let mut emitted_reasoning = false;
                for text in extract_reasoning_text(&event) {
                    emitted_reasoning = true;
                    on_event(ModelStreamEvent::ReasoningDelta(text))?;
                }

                if !emitted_reasoning {
                    if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                        on_event(ModelStreamEvent::ReasoningDelta(delta.to_string()))?;
                    } else if let Some(text) = event.get("text").and_then(Value::as_str) {
                        on_event(ModelStreamEvent::ReasoningDelta(text.to_string()))?;
                    }
                }
            }
            _ => {}
        }
    }

    if !emitted_text && !completed_text.trim().is_empty() {
        on_event(ModelStreamEvent::Delta(completed_text))?;
    }

    Ok(TurnOutcome {
        event_count,
        tool_calls,
        usage,
    })
}

pub(crate) fn read_chat_completions_sse_response<R, F>(
    response: R,
    cancel: &CancelToken,
    on_event: &mut F,
) -> Result<TurnOutcome>
where
    R: Read + Send + 'static,
    F: FnMut(ModelStreamEvent) -> Result<()>,
{
    let lines = spawn_sse_line_pump(response);
    read_chat_completions_sse_lines(
        &mut || next_pumped_line(&lines, cancel, "failed to read chat SSE line"),
        on_event,
    )
}

/// BufRead-based variant kept so fixture tests can drive the parser from a
/// Cursor without a pump thread (not cancellable mid-read).
#[cfg(test)]
pub(crate) fn read_chat_completions_sse_reader<R, F>(
    reader: R,
    on_event: &mut F,
) -> Result<TurnOutcome>
where
    R: BufRead,
    F: FnMut(ModelStreamEvent) -> Result<()>,
{
    let mut lines = reader.lines();
    read_chat_completions_sse_lines(
        &mut || match lines.next() {
            Some(line) => line.map(Some).wrap_err("failed to read chat SSE line"),
            None => Ok(None),
        },
        on_event,
    )
}

fn read_chat_completions_sse_lines<F>(
    next_line: &mut dyn FnMut() -> Result<Option<String>>,
    on_event: &mut F,
) -> Result<TurnOutcome>
where
    F: FnMut(ModelStreamEvent) -> Result<()>,
{
    let mut event_count = 0;
    let mut reasoning_content = String::new();
    let mut tool_calls: BTreeMap<usize, PartialChatToolCall> = BTreeMap::new();
    let mut usage = None;

    while let Some(line) = next_line()? {
        let Some(payload) = line.strip_prefix("data: ") else {
            continue;
        };

        if payload == "[DONE]" {
            break;
        }

        event_count += 1;
        let event = parse_sse_payload(payload)?;
        if let Some(error) = event.get("error") {
            bail!("{}", chat_backend_error_message(error));
        }

        // Usage rides on the final chunk (usually with an empty choices
        // array); keep the last one seen.
        if let Some(parsed) = event.get("usage").and_then(parse_token_usage) {
            usage = Some(parsed);
        }

        let Some(choices) = event.get("choices").and_then(Value::as_array) else {
            continue;
        };

        for choice in choices {
            let Some(delta) = choice.get("delta") else {
                continue;
            };

            if let Some(reasoning) = delta.get("reasoning_content").and_then(Value::as_str)
                && !reasoning.is_empty()
            {
                reasoning_content.push_str(reasoning);
                on_event(ModelStreamEvent::ReasoningDelta(reasoning.to_string()))?;
            }

            if let Some(content) = delta.get("content").and_then(Value::as_str)
                && !content.is_empty()
            {
                on_event(ModelStreamEvent::Delta(content.to_string()))?;
            }

            let Some(delta_tool_calls) = delta.get("tool_calls").and_then(Value::as_array) else {
                continue;
            };

            for (position, tool_call) in delta_tool_calls.iter().enumerate() {
                let index = tool_call
                    .get("index")
                    .and_then(Value::as_u64)
                    .map(|value| value as usize)
                    .unwrap_or(position);
                let entry = tool_calls.entry(index).or_default();

                if let Some(id) = tool_call.get("id").and_then(Value::as_str)
                    && !id.is_empty()
                {
                    entry.id = id.to_string();
                }

                let Some(function) = tool_call.get("function") else {
                    continue;
                };

                if let Some(name) = function.get("name").and_then(Value::as_str)
                    && !name.is_empty()
                {
                    entry.name = name.to_string();
                }

                if let Some(arguments) = function.get("arguments").and_then(Value::as_str) {
                    entry.arguments.push_str(arguments);
                }
            }
        }
    }

    let tool_calls = tool_calls
        .into_iter()
        .filter_map(|(index, call)| {
            if call.name.trim().is_empty() {
                return None;
            }

            Some(ToolCall {
                name: call.name,
                call_id: if call.id.trim().is_empty() {
                    format!("call_{index}")
                } else {
                    call.id
                },
                arguments: if call.arguments.trim().is_empty() {
                    "{}".to_string()
                } else {
                    call.arguments
                },
                reasoning_content: (!reasoning_content.trim().is_empty())
                    .then(|| reasoning_content.clone()),
            })
        })
        .collect();

    Ok(TurnOutcome {
        event_count,
        tool_calls,
        usage,
    })
}

/// Parse a usage object from either wire dialect: Responses-style
/// (input_tokens/output_tokens + input_tokens_details.cached_tokens) or
/// chat-completions-style (prompt_tokens/completion_tokens +
/// prompt_tokens_details.cached_tokens).
pub(crate) fn parse_token_usage(value: &Value) -> Option<TokenUsage> {
    let input = value
        .get("input_tokens")
        .or_else(|| value.get("prompt_tokens"))
        .and_then(Value::as_u64);
    let output = value
        .get("output_tokens")
        .or_else(|| value.get("completion_tokens"))
        .and_then(Value::as_u64);
    if input.is_none() && output.is_none() {
        return None;
    }

    let cached = value
        .get("input_tokens_details")
        .or_else(|| value.get("prompt_tokens_details"))
        .and_then(|details| details.get("cached_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);

    Some(TokenUsage {
        input: input.unwrap_or(0),
        output: output.unwrap_or(0),
        cached,
    })
}

pub(crate) fn chat_backend_error_message(error: &Value) -> String {
    let code = error
        .get("code")
        .and_then(Value::as_str)
        .unwrap_or("backend_error");
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("The chat backend failed before completing the turn.");
    format!(
        "model failed: {code} · {}",
        crate::model::exec::compact(message, 220)
    )
}

#[cfg(test)]
pub(crate) fn parse_sse_response(stream: &str) -> Result<ModelChatResult> {
    let mut text = String::new();
    let mut completed_text = String::new();
    let mut event_count = 0;

    for line in stream.lines() {
        let Some(payload) = line.strip_prefix("data: ") else {
            continue;
        };

        if payload == "[DONE]" {
            break;
        }

        event_count += 1;
        let event = parse_sse_payload(payload)?;
        match event.get("type").and_then(Value::as_str) {
            Some("response.output_text.delta") | Some("response.refusal.delta") => {
                if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                    text.push_str(delta);
                }
            }
            Some("response.output_text.done") => {
                if let Some(done_text) = event.get("text").and_then(Value::as_str) {
                    completed_text = done_text.to_string();
                }
            }
            Some("response.completed") => {
                if text.trim().is_empty()
                    && let Some(done_text) = extract_completed_output_text(&event)
                {
                    completed_text = done_text;
                }
                break;
            }
            _ => {}
        }
    }

    if text.trim().is_empty() {
        text = completed_text;
    }

    if text.trim().is_empty() {
        bail!("Codex backend completed without output text");
    }

    Ok(ModelChatResult {
        response: text.trim().to_string(),
        event_count,
    })
}

pub(crate) fn parse_sse_payload(payload: &str) -> Result<Value> {
    serde_json::from_str(payload).wrap_err("failed to parse SSE event")
}

pub(crate) fn backend_failure_message(event: &Value) -> String {
    let event_type = event
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("response.failed");
    let response = event.get("response").unwrap_or(event);
    let error = response
        .get("error")
        .or_else(|| event.get("error"))
        .unwrap_or(&Value::Null);
    let code = error
        .get("code")
        .and_then(Value::as_str)
        .unwrap_or("backend_error");
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("The model backend failed before completing the turn.");
    let normalized_code = code.replace(['_', '-'], "").to_ascii_lowercase();

    if normalized_code.contains("serverisoverloaded") || message.contains("currently overloaded") {
        return "model overloaded: Our servers are currently overloaded. Please try again later."
            .to_string();
    }

    let id = response
        .get("id")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty());
    let id_suffix = id.map_or(String::new(), |id| format!(" · {id}"));

    format!(
        "model failed: {event_type} · {code} · {}{id_suffix}",
        crate::model::exec::compact(message, 220)
    )
}

pub(crate) fn extract_tool_call(event: &Value) -> Option<ToolCall> {
    let item = event.get("item")?;
    if item.get("type")?.as_str()? != "function_call" {
        return None;
    }

    extract_tool_call_item(item)
}

pub(crate) fn extract_completed_tool_calls(event: &Value) -> Vec<ToolCall> {
    response_output_items(event)
        .into_iter()
        .filter_map(extract_tool_call_item)
        .collect()
}

fn extract_tool_call_item(item: &Value) -> Option<ToolCall> {
    if item.get("type")?.as_str()? != "function_call" {
        return None;
    }

    Some(ToolCall {
        name: item.get("name")?.as_str()?.to_string(),
        call_id: item.get("call_id")?.as_str()?.to_string(),
        arguments: item.get("arguments")?.as_str()?.to_string(),
        reasoning_content: None,
    })
}

pub(crate) fn extract_completed_output_text(event: &Value) -> Option<String> {
    let mut parts = Vec::new();

    for item in response_output_items(event) {
        if let Some(text) = item.get("text").and_then(Value::as_str) {
            parts.push(text);
        }

        let Some(content) = item.get("content").and_then(Value::as_array) else {
            continue;
        };

        for part in content {
            if let Some(text) = part
                .get("text")
                .or_else(|| part.get("refusal"))
                .and_then(Value::as_str)
            {
                parts.push(text);
            }
        }
    }

    let text = parts.join("");
    (!text.trim().is_empty()).then_some(text)
}

fn response_output_items(event: &Value) -> Vec<&Value> {
    event
        .get("response")
        .and_then(|response| response.get("output"))
        .or_else(|| event.get("output"))
        .and_then(Value::as_array)
        .map(|items| items.iter().collect())
        .unwrap_or_default()
}

pub(crate) fn extract_reasoning_text(event: &Value) -> Vec<String> {
    let mut parts = Vec::new();

    collect_reasoning_text(event, &mut parts);

    parts
        .into_iter()
        .filter(|part| !part.trim().is_empty())
        .collect()
}

fn collect_reasoning_text(value: &Value, parts: &mut Vec<String>) {
    match value {
        Value::Array(items) => {
            for item in items {
                collect_reasoning_text(item, parts);
            }
        }
        Value::Object(map) => {
            let value_type = map.get("type").and_then(Value::as_str).unwrap_or_default();
            let looks_like_reasoning = value_type.contains("reasoning")
                || value_type.contains("summary")
                || map.contains_key("summary")
                || map.contains_key("summary_text")
                || map.contains_key("reasoning");

            if looks_like_reasoning {
                for key in ["summary_text", "summary", "text", "delta"] {
                    if let Some(text) = map.get(key).and_then(Value::as_str) {
                        parts.push(text.to_string());
                    }
                }
            }

            if looks_like_reasoning {
                for key in ["summary", "content", "reasoning", "item", "output"] {
                    if let Some(child) = map.get(key) {
                        collect_reasoning_text(child, parts);
                    }
                }
            } else {
                for child in map.values() {
                    collect_reasoning_text(child, parts);
                }
            }
        }
        Value::String(_) | Value::Number(_) | Value::Bool(_) | Value::Null => {}
    }
}
