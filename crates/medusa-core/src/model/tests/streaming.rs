use super::*;

#[test]
fn parses_output_text_deltas() {
    let stream = r#"data: {"type":"response.created"}
data: {"type":"response.output_text.delta","delta":"hello"}
data: {"type":"response.output_text.delta","delta":" medusa"}
data: {"type":"response.completed"}
"#;

    let result = wire::parse_sse_response(stream).unwrap();

    assert_eq!(result.response, "hello medusa");
    assert_eq!(result.event_count, 4);
}

#[test]
fn parses_done_text_fallback() {
    let stream = r#"data: {"type":"response.output_text.done","text":"fallback"}
"#;

    let result = wire::parse_sse_response(stream).unwrap();

    assert_eq!(result.response, "fallback");
}

#[test]
fn parses_completed_response_output_text() {
    let stream = r#"data: {"type":"response.created"}
data: {"type":"response.completed","response":{"output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Hi there."}]}]}}
data: {"type":"response.keepalive"}
"#;

    let result = wire::parse_sse_response(stream).unwrap();

    assert_eq!(result.response, "Hi there.");
    assert_eq!(result.event_count, 2);
}

#[test]
fn backend_failure_message_hides_raw_failed_payload() {
    let event = json!({
        "type": "response.failed",
        "response": {
            "id": "resp_test",
            "instructions": "very long prompt should not appear",
            "error": {
                "code": "server_is_overloaded",
                "message": "Our servers are currently overloaded. Please try again later."
            }
        }
    });

    let message = wire::backend_failure_message(&event);

    assert_eq!(
        message,
        "model overloaded: Our servers are currently overloaded. Please try again later."
    );
    assert!(!message.contains("instructions"));
    assert!(!message.contains("response.failed"));
}

#[test]
fn extracts_reasoning_summary_delta_events() {
    let event = json!({
        "type": "response.reasoning_summary_text.delta",
        "delta": "Checking files"
    });

    assert_eq!(wire::extract_reasoning_text(&event), vec!["Checking files"]);
}

#[test]
fn extracts_completed_reasoning_summaries() {
    let event = json!({
        "type": "response.completed",
        "response": {
            "output": [{
                "type": "reasoning",
                "summary": [{"type": "summary_text", "text": "Inspected the render loop."}]
            }]
        }
    });

    assert_eq!(
        wire::extract_reasoning_text(&event),
        vec!["Inspected the render loop."]
    );
}

#[test]
fn extracts_completed_tool_calls() {
    let event = json!({
        "type": "response.completed",
        "response": {
            "output": [{
                "type": "function_call",
                "name": "terminal_exec",
                "call_id": "call_1",
                "arguments": "{\"command\":\"pwd\"}"
            }]
        }
    });

    let calls = wire::extract_completed_tool_calls(&event);

    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].name, "terminal_exec");
    assert_eq!(calls[0].call_id, "call_1");
}

#[test]
fn patch_recovery_temporarily_withholds_patch_tool() {
    let all_tools = schema::medusa_tools(true, true, &[]);
    let recovery_tools = schema::medusa_tools(false, false, &[]);

    assert!(
        all_tools
            .iter()
            .any(|tool| tool.get("name") == Some(&json!("file_patch")))
    );
    assert!(
        !recovery_tools
            .iter()
            .any(|tool| tool.get("name") == Some(&json!("file_patch")))
    );
}

#[test]
fn medusa_tools_include_structured_file_tools() {
    let tools = schema::medusa_tools(true, true, &[]);
    let names = tools
        .iter()
        .filter_map(|tool| tool.get("name").and_then(Value::as_str))
        .collect::<Vec<_>>();

    assert!(names.contains(&"file_read"));
    assert!(names.contains(&"file_search"));
    assert!(names.contains(&"fs_list"));
    assert!(names.contains(&"explore_batch"));
    assert!(names.contains(&"file_edit"));
    assert!(names.contains(&"file_patch"));
    assert!(names.contains(&"terminal_exec"));
    assert!(names.contains(&"plan_update"));
    assert!(names.contains(&"decision_request"));
}

#[test]
fn workflow_run_description_lists_named_agents_only_when_present() {
    let workflow_description = |agent_names: &[String]| {
        schema::medusa_tools(true, true, agent_names)
            .iter()
            .find(|tool| tool.get("name") == Some(&json!("workflow_run")))
            .and_then(|tool| tool.get("description"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .expect("workflow_run tool present")
    };

    let named = workflow_description(&["reviewer".to_string(), "mapper".to_string()]);
    assert!(named.contains("agentType"));
    assert!(named.contains("reviewer, mapper"));

    let unnamed = workflow_description(&[]);
    assert!(!unnamed.contains("Named agents from .medusa/agents"));
}

#[test]
fn web_tools_offered_even_in_read_only_turns() {
    // Side-effect-free web tools stay available when mutation tools are withheld.
    for tools in [
        schema::medusa_tools(true, true, &[]),
        schema::medusa_tools(false, false, &[]),
    ] {
        let names = tools
            .iter()
            .filter_map(|tool| tool.get("name").and_then(Value::as_str))
            .collect::<Vec<_>>();
        assert!(names.contains(&"web_fetch"));
        assert!(names.contains(&"web_search"));
    }
}

#[test]
fn web_tools_classified_read_only_and_display_dotted_names() {
    assert!(exec::tool_call_is_read_only("web_fetch"));
    assert!(exec::tool_call_is_read_only("web_search"));
    assert!(!exec::tool_call_is_file_mutation("web_fetch"));
    assert_eq!(exec::display_tool_name("web_fetch"), "web.fetch");
    assert_eq!(exec::display_tool_name("web_search"), "web.search");
}

#[test]
fn web_tool_calls_summarize_and_guard_bad_urls() {
    let fetch_call = types::ToolCall {
        name: "web_fetch".to_string(),
        call_id: "call_web_1".to_string(),
        arguments: json!({"url": "http://localhost:8080/admin"}).to_string(),
        reasoning_content: None,
    };
    assert_eq!(
        exec::summarize_tool_call(&fetch_call),
        "fetch http://localhost:8080/admin"
    );

    let search_call = types::ToolCall {
        name: "web_search".to_string(),
        call_id: "call_web_2".to_string(),
        arguments: json!({"query": "rust lifetimes"}).to_string(),
        reasoning_content: None,
    };
    assert_eq!(
        exec::summarize_tool_call(&search_call),
        "web \"rust lifetimes\""
    );

    // The loopback guard fires before any network access, so this is
    // deterministic and offline.
    let workspace = temp_workspace();
    let tools = ToolRuntime::new(&workspace).unwrap();
    let execution = exec::execute_tool_call(
        &tools,
        &fetch_call,
        &types::ToolLoopState::default(),
        types::ToolLoopPolicy::read_only(),
    );
    assert!(execution.failed);
    assert!(
        execution.output.contains("local host"),
        "unexpected output: {}",
        execution.output
    );
    fs::remove_dir_all(&workspace).ok();
}

#[test]
fn web_tools_are_egress_gated_by_permission_mode() {
    use crate::tools::{ApprovalDecision, ApprovalRequest, ApprovalTool};
    use std::sync::{Arc, Mutex};

    let fetch = |url: &str| types::ToolCall {
        name: "web_fetch".to_string(),
        call_id: "call_web_gate".to_string(),
        arguments: json!({ "url": url }).to_string(),
        reasoning_content: None,
    };
    let run = |tools: &ToolRuntime, call: &types::ToolCall| {
        exec::execute_tool_call(
            tools,
            call,
            &types::ToolLoopState::default(),
            types::ToolLoopPolicy::read_only(),
        )
    };
    // A private IP: the SSRF URL guard rejects it *after* the egress gate, so
    // the failure message reveals which layer fired — no network is touched.
    let target = "http://127.0.0.1/";

    // Open mode: egress auto-allowed → the request reaches the URL guard.
    let open_ws = temp_workspace();
    crate::permissions::PermissionPolicy::write_mode(
        &open_ws,
        crate::permissions::PermissionMode::Open,
    )
    .unwrap();
    let open = run(&ToolRuntime::new(&open_ws).unwrap(), &fetch(target));
    assert!(open.failed);
    assert!(
        open.output.contains("refusing") && !open.output.contains("requires approval"),
        "Open must pass the gate and hit the URL guard: {}",
        open.output
    );

    // Guarded mode, no approver: egress denied before any fetch.
    let guarded_ws = temp_workspace();
    crate::permissions::PermissionPolicy::write_mode(
        &guarded_ws,
        crate::permissions::PermissionMode::Guarded,
    )
    .unwrap();
    let guarded_tools = ToolRuntime::new(&guarded_ws).unwrap();
    let denied = run(&guarded_tools, &fetch(target));
    assert!(denied.failed);
    assert!(
        denied.output.contains("requires approval"),
        "Guarded without an approver must be denied: {}",
        denied.output
    );
    // web_search shares the same gate.
    let search = types::ToolCall {
        name: "web_search".to_string(),
        call_id: "call_web_gate_2".to_string(),
        arguments: json!({ "query": "anything" }).to_string(),
        reasoning_content: None,
    };
    let denied_search = run(&guarded_tools, &search);
    assert!(denied_search.failed);
    assert!(
        denied_search.output.contains("requires approval"),
        "Guarded web_search must be denied without an approver: {}",
        denied_search.output
    );

    // Guarded mode with an approver that allows: the gate is consulted, the
    // request carries the URL, and control reaches the URL guard.
    let seen = Arc::new(Mutex::new(Vec::<ApprovalRequest>::new()));
    let seen_clone = Arc::clone(&seen);
    let approver = ToolRuntime::new(&guarded_ws)
        .unwrap()
        .with_approval_handler(Arc::new(move |request: ApprovalRequest| {
            seen_clone.lock().unwrap().push(request);
            ApprovalDecision::AllowOnce
        }));
    let allowed = run(&approver, &fetch(target));
    assert!(allowed.failed);
    assert!(
        allowed.output.contains("refusing"),
        "approved egress should reach the URL guard: {}",
        allowed.output
    );
    let requests = seen.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].tool, ApprovalTool::WebFetch);
    assert_eq!(requests[0].command.as_deref(), Some(target));

    for ws in [open_ws, guarded_ws] {
        fs::remove_dir_all(&ws).ok();
    }
}

#[test]
fn workflow_tool_only_offered_to_main_turns() {
    let main_tools = schema::medusa_tools(true, true, &[]);
    let subagent_tools = schema::medusa_tools(true, false, &[]);

    assert!(
        main_tools
            .iter()
            .any(|tool| tool.get("name") == Some(&json!("workflow_run")))
    );
    assert!(
        !subagent_tools
            .iter()
            .any(|tool| tool.get("name") == Some(&json!("workflow_run")))
    );
}

#[test]
fn workflow_run_rejected_for_subagents_and_bad_args() {
    let workspace = temp_workspace();
    let tools = ToolRuntime::new(&workspace).unwrap();
    let backend = super::DirectCodexBackend::new(&workspace).unwrap();
    let mut events = Vec::new();
    let mut on_event = |event: types::ModelStreamEvent| {
        events.push(event);
        Ok(())
    };

    let nested_call = types::ToolCall {
        name: "workflow_run".to_string(),
        call_id: "call_wf".to_string(),
        arguments: r#"{"script":"return 1;"}"#.to_string(),
        reasoning_content: None,
    };
    let nested = exec::execute_workflow_run_with_hooks(
        &tools,
        &nested_call,
        HarnessPolicy::for_user_prompt("fix tests"),
        types::ToolLoopPolicy::subagent(true),
        &backend,
        &mut on_event,
    );
    assert!(nested.failed);
    assert!(nested.output.contains("nested"));

    let missing_script = types::ToolCall {
        name: "workflow_run".to_string(),
        call_id: "call_wf2".to_string(),
        arguments: r#"{"goal":"no script"}"#.to_string(),
        reasoning_content: None,
    };
    let missing = exec::execute_workflow_run_with_hooks(
        &tools,
        &missing_script,
        HarnessPolicy::for_user_prompt("fix tests"),
        types::ToolLoopPolicy::mutation_allowed(),
        &backend,
        &mut on_event,
    );
    assert!(missing.failed);
    assert!(missing.output.contains("script is required"));
}

#[test]
fn retry_helpers_classify_and_back_off() {
    assert!(super::retryable_status(429));
    assert!(super::retryable_status(500));
    assert!(super::retryable_status(504));
    assert!(!super::retryable_status(400));
    assert!(!super::retryable_status(401));
    assert!(!super::retryable_status(404));

    assert_eq!(
        super::retry_backoff(1, None),
        std::time::Duration::from_millis(1_000)
    );
    assert_eq!(
        super::retry_backoff(2, None),
        std::time::Duration::from_millis(2_000)
    );
    assert_eq!(
        super::retry_backoff(1, Some(5)),
        std::time::Duration::from_secs(5)
    );
    assert_eq!(
        super::retry_backoff(1, Some(600)),
        std::time::Duration::from_secs(30)
    );
}

#[test]
fn provider_registry_upgrades_legacy_model_ids() {
    let registry = crate::model::provider::ProviderRegistry::builtins();
    assert_eq!(
        registry
            .select("deepseek-v4-flash", None)
            .unwrap()
            .as_string(),
        "deepseek/deepseek-v4-flash"
    );
    assert_eq!(
        registry.select("gpt-5.5", None).unwrap().as_string(),
        "codex/gpt-5.5"
    );
    assert_eq!(
        registry
            .select("custom-model", Some("ollama"))
            .unwrap()
            .as_string(),
        "ollama/custom-model"
    );
}

#[test]
fn deepseek_reasoning_effort_maps_codex_names_to_deepseek_values() {
    assert_eq!(schema::deepseek_reasoning_effort("xhigh"), "max");
    assert_eq!(schema::deepseek_reasoning_effort("max"), "max");
    assert_eq!(schema::deepseek_reasoning_effort("ultra"), "max");
    assert_eq!(schema::deepseek_reasoning_effort("medium"), "high");
    assert_eq!(schema::deepseek_reasoning_effort("low"), "high");
}

#[test]
fn chat_reasoning_dialects_emit_provider_specific_fields() {
    use crate::model::provider::ThinkingDialect;

    let mut openai = json!({});
    schema::apply_chat_reasoning(&mut openai, ThinkingDialect::Openai, "high");
    assert_eq!(openai["reasoning_effort"], json!("high"));

    let mut openrouter = json!({});
    schema::apply_chat_reasoning(&mut openrouter, ThinkingDialect::Openrouter, "xhigh");
    assert_eq!(openrouter["reasoning"]["effort"], json!("xhigh"));

    let mut deepseek = json!({});
    schema::apply_chat_reasoning(&mut deepseek, ThinkingDialect::Deepseek, "xhigh");
    assert_eq!(deepseek["thinking"]["type"], json!("enabled"));
    assert_eq!(deepseek["reasoning_effort"], json!("max"));

    let mut disabled = json!({});
    schema::apply_chat_reasoning(&mut disabled, ThinkingDialect::Openrouter, "none");
    assert_eq!(disabled, json!({}));
}

#[test]
fn codex_ultra_uses_max_as_the_wire_reasoning_effort() {
    assert_eq!(schema::codex_reasoning_effort("ultra"), "max");
    assert_eq!(schema::codex_reasoning_effort("ULTRA"), "max");
    assert_eq!(schema::codex_reasoning_effort("max"), "max");
    assert_eq!(schema::codex_reasoning_effort("xhigh"), "xhigh");
    assert_eq!(schema::codex_reasoning_effort("none"), "none");
}

#[test]
fn ultra_adds_proactive_orchestration_only_to_workflow_capable_turns() {
    let context =
        super::with_ultra_orchestration_context(Some("project context".to_string()), "ultra", true)
            .expect("ultra context");
    assert!(context.contains("project context"));
    assert!(context.contains("Ultra orchestration mode is active"));
    assert!(context.contains("workflow_run"));

    assert_eq!(
        super::with_ultra_orchestration_context(Some("base".to_string()), "high", true),
        Some("base".to_string())
    );
    assert_eq!(
        super::with_ultra_orchestration_context(Some("base".to_string()), "ultra", false),
        Some("base".to_string())
    );
}

#[test]
fn chat_completion_tools_use_openai_compatible_schema() {
    let tools = schema::chat_completion_tools(true, true, &[], &[]);
    let file_read = tools
        .iter()
        .find(|tool| {
            tool.get("function")
                .and_then(|function| function.get("name"))
                == Some(&json!("file_read"))
        })
        .expect("file_read tool");

    assert_eq!(file_read.get("type"), Some(&json!("function")));
    assert!(file_read.get("name").is_none());
    assert!(
        file_read
            .get("function")
            .and_then(|function| function.get("parameters"))
            .is_some()
    );
}

#[test]
fn extra_mcp_tools_merge_into_both_provider_schemas() {
    let mcp_tool = json!({
        "type": "function",
        "name": "mcp_docs_search",
        "description": "(MCP tool from server `docs`) Search docs",
        "parameters": { "type": "object" },
    });

    // Responses-style bodies append the schema verbatim.
    let mut codex_tools = schema::medusa_tools(true, true, &[]);
    codex_tools.extend_from_slice(std::slice::from_ref(&mcp_tool));
    assert!(
        codex_tools
            .iter()
            .any(|tool| tool.get("name") == Some(&json!("mcp_docs_search")))
    );

    // Chat-completions bodies re-wrap it in the {type, function} envelope.
    let chat_tools =
        schema::chat_completion_tools(true, true, &[], std::slice::from_ref(&mcp_tool));
    let wrapped = chat_tools
        .iter()
        .find(|tool| {
            tool.get("function")
                .and_then(|function| function.get("name"))
                == Some(&json!("mcp_docs_search"))
        })
        .expect("mcp tool wrapped for chat completions");
    assert_eq!(wrapped.get("type"), Some(&json!("function")));
    assert_eq!(
        wrapped
            .get("function")
            .and_then(|function| function.get("parameters")),
        Some(&json!({ "type": "object" }))
    );
    assert!(
        chat_tools.len() > schema::chat_completion_tools(true, true, &[], &[]).len(),
        "extra tools must extend, not replace, the built-in surface"
    );
}

#[test]
fn chat_completion_messages_convert_responses_tool_items() {
    let messages = wire::chat_completion_messages_from_input(
        vec![
            json!({"role": "user", "content": "read README"}),
            json!({
                "type": "function_call",
                "call_id": "call_1",
                "name": "file_read",
                "arguments": "{\"paths\":[\"README.md\"]}",
            }),
            json!({
                "type": "function_call_output",
                "call_id": "call_1",
                "output": "README contents",
            }),
        ],
        "system instructions",
    );

    assert_eq!(messages[0].get("role"), Some(&json!("system")));
    assert_eq!(messages[1].get("role"), Some(&json!("user")));
    assert_eq!(messages[2].get("role"), Some(&json!("assistant")));
    assert_eq!(messages[3].get("role"), Some(&json!("tool")));
    assert_eq!(messages[3].get("tool_call_id"), Some(&json!("call_1")));
}

#[test]
fn chat_completion_messages_coalesce_parallel_calls_and_preserve_reasoning_details() {
    let details = json!([{
        "type": "reasoning.summary",
        "summary": "Inspect both files",
        "id": "reasoning-1",
        "format": "anthropic-claude-v1",
        "index": 0
    }]);
    let messages = wire::chat_completion_messages_from_input(
        vec![
            json!({"role": "user", "content": "inspect both"}),
            json!({
                "type": "function_call",
                "call_id": "call_1",
                "name": "file_read",
                "arguments": "{\"paths\":[\"README.md\"]}",
                "reasoning_details": details,
            }),
            json!({
                "type": "function_call",
                "call_id": "call_2",
                "name": "file_read",
                "arguments": "{\"paths\":[\"Cargo.toml\"]}",
                "reasoning_details": details,
            }),
            json!({"type": "function_call_output", "call_id": "call_1", "output": "a"}),
            json!({"type": "function_call_output", "call_id": "call_2", "output": "b"}),
        ],
        "system",
    );

    assert_eq!(messages.len(), 5);
    assert_eq!(messages[2]["tool_calls"].as_array().unwrap().len(), 2);
    assert_eq!(messages[2]["reasoning_details"], details);
    assert_eq!(messages[3]["tool_call_id"], json!("call_1"));
    assert_eq!(messages[4]["tool_call_id"], json!("call_2"));
}

#[test]
fn chat_completion_messages_preserve_images_when_provider_supports_them() {
    let messages = wire::chat_completion_messages_from_input_with_images(
        vec![json!({
            "role": "user",
            "content": [
                {"type": "input_text", "text": "inspect this"},
                {"type": "input_image", "image_url": "data:image/png;base64,AA=="}
            ]
        })],
        "system",
        true,
    );

    assert_eq!(
        messages[1]["content"][1]["image_url"]["url"],
        json!("data:image/png;base64,AA==")
    );
}

#[test]
fn chat_completion_messages_preserve_deepseek_reasoning_content() {
    let messages = wire::chat_completion_messages_from_input(
        vec![
            json!({"role": "user", "content": "read README"}),
            json!({
                "type": "function_call",
                "call_id": "call_1",
                "name": "file_read",
                "arguments": "{\"paths\":[\"README.md\"]}",
                "reasoning_content": "I need to inspect the README first.",
            }),
            json!({
                "type": "function_call_output",
                "call_id": "call_1",
                "output": "README contents",
            }),
        ],
        "system instructions",
    );

    assert_eq!(
        messages[2].get("reasoning_content"),
        Some(&json!("I need to inspect the README first."))
    );
}

#[test]
fn parses_chat_completion_stream_deltas_reasoning_and_tool_calls() {
    let stream = [
        format!(
            "data: {}\n",
            json!({
                "choices": [{
                    "delta": {
                        "reasoning_content": "checking",
                        "content": "Hi ",
                        "tool_calls": [{
                            "index": 0,
                            "id": "call_1",
                            "type": "function",
                            "function": {
                                "name": "file_read",
                                "arguments": "{\"paths\"",
                            }
                        }]
                    }
                }]
            })
        ),
        format!(
            "data: {}\n",
            json!({
                "choices": [{
                    "delta": {
                        "content": "there",
                        "tool_calls": [{
                            "index": 0,
                            "function": {
                                "arguments": ":[\"README.md\"]}",
                            }
                        }]
                    }
                }]
            })
        ),
        "data: [DONE]\n".to_string(),
    ]
    .join("");
    let mut events = Vec::new();

    let outcome =
        wire::read_chat_completions_sse_reader(std::io::Cursor::new(stream), &mut |event| {
            events.push(event);
            Ok(())
        })
        .unwrap();

    assert_eq!(outcome.event_count, 2);
    assert_eq!(outcome.tool_calls.len(), 1);
    assert_eq!(outcome.tool_calls[0].name, "file_read");
    assert_eq!(outcome.tool_calls[0].call_id, "call_1");
    assert_eq!(
        outcome.tool_calls[0].arguments,
        "{\"paths\":[\"README.md\"]}"
    );
    assert_eq!(
        outcome.tool_calls[0].reasoning_content.as_deref(),
        Some("checking")
    );
    assert!(events.contains(&types::ModelStreamEvent::ReasoningDelta(
        "checking".to_string()
    )));
    assert!(events.contains(&types::ModelStreamEvent::Delta("Hi ".to_string())));
    assert!(events.contains(&types::ModelStreamEvent::Delta("there".to_string())));
}

#[test]
fn parses_openrouter_reasoning_details_for_tool_continuations() {
    let detail = json!({
        "type": "reasoning.summary",
        "summary": "Need repository context",
        "id": "reasoning-1",
        "format": "anthropic-claude-v1",
        "index": 0
    });
    let stream = format!(
        "data: {}\ndata: [DONE]\n",
        json!({
            "choices": [{
                "delta": {
                    "reasoning_details": [detail.clone()],
                    "tool_calls": [{
                        "index": 0,
                        "id": "call_1",
                        "function": {"name": "fs_list", "arguments": "{}"}
                    }]
                }
            }]
        })
    );
    let mut events = Vec::new();

    let outcome =
        wire::read_chat_completions_sse_reader(std::io::Cursor::new(stream), &mut |event| {
            events.push(event);
            Ok(())
        })
        .unwrap();

    assert_eq!(outcome.reasoning_details, Some(vec![detail]));
    assert!(events.contains(&types::ModelStreamEvent::ReasoningDelta(
        "Need repository context".to_string()
    )));
}

#[test]
fn responses_stream_captures_usage_from_response_completed() {
    let stream = concat!(
        "data: {\"type\":\"response.created\"}\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"output\":[],\"usage\":{\"input_tokens\":1200,\"output_tokens\":45,\"input_tokens_details\":{\"cached_tokens\":800}}}}\n",
    );

    let outcome = wire::read_sse_response(
        std::io::Cursor::new(stream.as_bytes().to_vec()),
        &crate::cancel::CancelToken::new(),
        &mut |_event| Ok(()),
    )
    .unwrap();

    assert_eq!(
        outcome.usage,
        Some(types::TokenUsage {
            input: 1200,
            output: 45,
            cached: 800,
        })
    );
}

#[test]
fn chat_completions_stream_captures_usage_from_final_chunk() {
    let stream = [
        format!(
            "data: {}\n",
            json!({"choices": [{"delta": {"content": "hi"}}]})
        ),
        format!(
            "data: {}\n",
            json!({
                "choices": [],
                "usage": {
                    "prompt_tokens": 321,
                    "completion_tokens": 12,
                    "prompt_tokens_details": {"cached_tokens": 256}
                }
            })
        ),
        "data: [DONE]\n".to_string(),
    ]
    .join("");

    let outcome =
        wire::read_chat_completions_sse_reader(std::io::Cursor::new(stream), &mut |_event| Ok(()))
            .unwrap();

    assert_eq!(
        outcome.usage,
        Some(types::TokenUsage {
            input: 321,
            output: 12,
            cached: 256,
        })
    );
}

#[test]
fn parse_token_usage_accepts_both_field_families_and_rejects_junk() {
    let responses_style = json!({
        "input_tokens": 10,
        "output_tokens": 3,
        "input_tokens_details": {"cached_tokens": 4}
    });
    assert_eq!(
        wire::parse_token_usage(&responses_style),
        Some(types::TokenUsage {
            input: 10,
            output: 3,
            cached: 4
        })
    );

    let chat_style = json!({"prompt_tokens": 7, "completion_tokens": 2});
    assert_eq!(
        wire::parse_token_usage(&chat_style),
        Some(types::TokenUsage {
            input: 7,
            output: 2,
            cached: 0
        })
    );

    let deepseek_style = json!({
        "prompt_tokens": 100,
        "completion_tokens": 5,
        "prompt_cache_hit_tokens": 72,
        "prompt_cache_miss_tokens": 28
    });
    assert_eq!(
        wire::parse_token_usage(&deepseek_style),
        Some(types::TokenUsage {
            input: 100,
            output: 5,
            cached: 72
        })
    );

    // Only one side reported still counts; a usage-shaped object with
    // neither token family does not.
    let output_only = json!({"completion_tokens": 9});
    assert_eq!(
        wire::parse_token_usage(&output_only),
        Some(types::TokenUsage {
            input: 0,
            output: 9,
            cached: 0
        })
    );
    assert_eq!(wire::parse_token_usage(&json!({"other": 1})), None);
}

#[test]
fn token_usage_reports_cache_hit_rate_and_uncached_input() {
    let usage = types::TokenUsage {
        input: 1_000,
        output: 50,
        cached: 800,
    };

    assert_eq!(usage.uncached_input(), 200);
    assert_eq!(usage.cache_hit_percent(), Some(80.0));
    assert_eq!(types::TokenUsage::default().cache_hit_percent(), None);
}

#[test]
fn token_usage_add_accumulates_across_requests() {
    let mut turn = types::TokenUsage::default();
    turn.add(types::TokenUsage {
        input: 100,
        output: 10,
        cached: 60,
    });
    turn.add(types::TokenUsage {
        input: 250,
        output: 25,
        cached: 200,
    });

    assert_eq!(
        turn,
        types::TokenUsage {
            input: 350,
            output: 35,
            cached: 260
        }
    );
    assert_eq!(turn.total(), 385);
}
