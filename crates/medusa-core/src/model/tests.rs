use super::{exec, schema, types, wire};
use crate::harness::HarnessPolicy;
use crate::tools::ToolRuntime;
use serde_json::{Value, json};
use std::{fs, path::Path};

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
fn model_provider_is_inferred_from_common_model_ids() {
    assert_eq!(
        types::ModelProvider::infer_from_model("deepseek-v4-flash"),
        Some(types::ModelProvider::DeepSeek)
    );
    assert_eq!(
        types::ModelProvider::infer_from_model("gpt-5.5"),
        Some(types::ModelProvider::Codex)
    );
    assert_eq!(types::ModelProvider::infer_from_model("custom-model"), None);
}

#[test]
fn deepseek_reasoning_effort_maps_codex_names_to_deepseek_values() {
    assert_eq!(schema::deepseek_reasoning_effort("xhigh"), "max");
    assert_eq!(schema::deepseek_reasoning_effort("max"), "max");
    assert_eq!(schema::deepseek_reasoning_effort("medium"), "high");
    assert_eq!(schema::deepseek_reasoning_effort("low"), "high");
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

#[test]
fn mutation_tools_gated_by_policy_not_turn_mode() {
    let mutation_tools = schema::medusa_tools(true, true, &[]);
    let read_only_tools = schema::medusa_tools(false, false, &[]);

    assert!(
        mutation_tools
            .iter()
            .any(|tool| tool.get("name") == Some(&json!("question")))
    );
    assert!(
        mutation_tools
            .iter()
            .any(|tool| tool.get("name") == Some(&json!("file_patch")))
    );
    assert!(
        mutation_tools
            .iter()
            .any(|tool| tool.get("name") == Some(&json!("file_edit")))
    );
    assert!(
        !read_only_tools
            .iter()
            .any(|tool| tool.get("name") == Some(&json!("file_patch")))
    );
    assert!(
        !read_only_tools
            .iter()
            .any(|tool| tool.get("name") == Some(&json!("file_edit")))
    );
}

#[test]
fn medusa_instructions_include_explicit_skill_context() {
    let instructions = schema::medusa_instructions(
        Path::new("/workspace"),
        &types::ToolLoopState::default(),
        HarnessPolicy::for_user_prompt("use $review"),
        Some("Active Medusa skills.\n<skill name=\"review\">lead with findings</skill>"),
        false,
    );

    assert!(instructions.contains("Active Medusa skills"));
    assert!(instructions.contains("lead with findings"));
}

#[test]
fn medusa_instructions_mention_mcp_only_when_mcp_tools_are_active() {
    let base = |mcp_active| {
        schema::medusa_instructions(
            Path::new("/workspace"),
            &types::ToolLoopState::default(),
            HarnessPolicy::for_user_prompt("hello"),
            None,
            mcp_active,
        )
    };

    assert!(base(true).contains("mcp_<server>_<tool>"));
    assert!(!base(false).contains("mcp_<server>_<tool>"));
}

#[test]
fn context_compaction_keeps_recent_messages() {
    let messages = vec![
        types::ConversationMessage {
            role: "user".to_string(),
            content: "old old old old old".to_string(),
            attachments: Vec::new(),
        },
        types::ConversationMessage {
            role: "assistant".to_string(),
            content: "middle middle middle".to_string(),
            attachments: Vec::new(),
        },
        types::ConversationMessage {
            role: "user".to_string(),
            content: "latest".to_string(),
            attachments: Vec::new(),
        },
    ];

    let compacted = wire::compact_conversation_context(&messages, 10);

    assert!(compacted[0].content.contains("context compaction omitted"));
    assert_eq!(compacted.last().unwrap().content, "latest");
    assert!(
        compacted
            .iter()
            .all(|message| message.content != "old old old old old")
    );
}

#[test]
fn context_compaction_preserves_system_state_messages() {
    let messages = vec![
        types::ConversationMessage {
            role: "system".to_string(),
            content: "permissions".to_string(),
            attachments: Vec::new(),
        },
        types::ConversationMessage {
            role: "system".to_string(),
            content: "rolling session state".to_string(),
            attachments: Vec::new(),
        },
        types::ConversationMessage {
            role: "user".to_string(),
            content: "old old old old old".to_string(),
            attachments: Vec::new(),
        },
        types::ConversationMessage {
            role: "assistant".to_string(),
            content: "middle middle middle".to_string(),
            attachments: Vec::new(),
        },
        types::ConversationMessage {
            role: "user".to_string(),
            content: "latest".to_string(),
            attachments: Vec::new(),
        },
    ];

    let compacted = wire::compact_conversation_context(&messages, 10);

    assert_eq!(compacted[0].content, "permissions");
    assert_eq!(compacted[1].content, "rolling session state");
    assert!(
        compacted
            .iter()
            .any(|message| message.content.contains("context compaction omitted"))
    );
    assert_eq!(compacted.last().unwrap().content, "latest");
}

#[test]
fn question_tool_returns_user_facing_question() {
    let workspace = temp_workspace();
    let tools = ToolRuntime::new(&workspace).unwrap();
    let call = types::ToolCall {
        name: "question".to_string(),
        call_id: "call_test".to_string(),
        arguments: r#"{"question":"Which branch should I keep?"}"#.to_string(),
        reasoning_content: None,
    };

    let execution = exec::execute_tool_call(
        &tools,
        &call,
        &types::ToolLoopState::default(),
        types::ToolLoopPolicy::mutation_allowed(),
    );

    assert!(!execution.failed);
    assert!(execution.output.contains("Which branch should I keep?"));
}

#[test]
fn decision_request_tool_returns_structured_queue() {
    let workspace = temp_workspace();
    let tools = ToolRuntime::new(&workspace).unwrap();
    let call = types::ToolCall {
        name: "decision_request".to_string(),
        call_id: "call_test".to_string(),
        arguments: r#"{"title":"Choose UI","reason":"The layout affects planning flow.","questions":[{"id":"rail","prompt":"Show decisions in the right rail?","kind":"choice","options":["yes","no"],"recommended":"yes","required":true}],"assumptions":["Use the right rail if unanswered."]}"#.to_string(),
        reasoning_content: None,
    };

    let execution = exec::execute_tool_call(
        &tools,
        &call,
        &types::ToolLoopState::default(),
        types::ToolLoopPolicy::mutation_allowed(),
    );

    assert!(!execution.failed);
    assert!(execution.output.contains("\"title\":\"Choose UI\""));
    assert!(execution.output.contains("\"id\":\"rail\""));
    assert!(execution.output.contains("\"recommended\":\"yes\""));
}

fn read_call(call_id: &str, path: &str) -> types::ToolCall {
    types::ToolCall {
        name: "file_read".to_string(),
        call_id: call_id.to_string(),
        arguments: format!(r#"{{"paths":["{path}"]}}"#),
        reasoning_content: None,
    }
}

#[test]
fn read_only_tool_calls_execute_in_parallel_and_return_in_emission_order() {
    let workspace = temp_workspace();
    fs::write(workspace.join("a.txt"), "alpha\n").unwrap();
    fs::write(workspace.join("b.txt"), "beta\n").unwrap();
    let tools = ToolRuntime::new(&workspace).unwrap();
    let backend = super::DirectCodexBackend::new(&workspace).unwrap();

    let calls = vec![
        read_call("call_a", "a.txt"),
        read_call("call_b", "b.txt"),
        read_call("call_missing", "missing.txt"),
    ];

    let mut result_ids = Vec::new();
    let mut state = types::ToolLoopState::default();
    let executions = backend
        .execute_turn_tool_calls(
            &tools,
            &calls,
            &mut state,
            crate::harness::HarnessPolicy::for_user_prompt("read the files"),
            types::ToolLoopPolicy::mutation_allowed(),
            &mut |event| {
                if let types::ModelStreamEvent::ToolResult { call_id, .. } = event {
                    result_ids.push(call_id);
                }
                Ok(())
            },
        )
        .unwrap();

    // Executions map back to emission order even though completion order
    // is nondeterministic.
    assert_eq!(executions.len(), 3);
    assert!(!executions[0].failed);
    assert!(executions[0].output.contains("alpha"));
    assert!(!executions[1].failed);
    assert!(executions[1].output.contains("beta"));
    assert!(executions[2].failed);

    // Every call produced exactly one result event, keyed by call_id.
    result_ids.sort();
    assert_eq!(result_ids, vec!["call_a", "call_b", "call_missing"]);
}

fn edit_call(call_id: &str, path: &str, old: &str, new: &str) -> types::ToolCall {
    types::ToolCall {
        name: "file_edit".to_string(),
        call_id: call_id.to_string(),
        arguments: serde_json::json!({"path": path, "oldString": old, "newString": new})
            .to_string(),
        reasoning_content: None,
    }
}

#[test]
fn verification_runs_once_after_the_last_mutation_and_covers_the_whole_turn() {
    let workspace = temp_workspace();
    fs::write(workspace.join("first.py"), "x = 1\n").unwrap();
    fs::write(workspace.join("second.py"), "y = 2\n").unwrap();
    let tools = ToolRuntime::new(&workspace).unwrap();
    let backend = super::DirectCodexBackend::new(&workspace).unwrap();

    // First edit breaks first.py; second edit touches second.py harmlessly.
    let calls = vec![
        edit_call("call_1", "first.py", "x = 1\n", "def broken(:\n"),
        edit_call("call_2", "second.py", "y = 2\n", "y = 3\n"),
    ];

    let mut state = types::ToolLoopState::default();
    let executions = backend
        .execute_turn_tool_calls(
            &tools,
            &calls,
            &mut state,
            crate::harness::HarnessPolicy::for_user_prompt("edit the files"),
            types::ToolLoopPolicy::mutation_allowed(),
            &mut |_| Ok(()),
        )
        .unwrap();

    // Mid-batch edits carry no verify block; only the final mutation does.
    assert!(!executions[0].output.contains("verify:"));
    assert!(
        executions[1]
            .output
            .contains("verify: python py_compile FAILED")
    );
    // The check covered the earlier edit's file, not just the last one.
    assert!(executions[1].output.contains("first.py"));
    // The edit itself still succeeded — verification is feedback, not a veto.
    assert!(!executions[1].failed);

    // The verify block reaches the model context and the UI summary.
    let context = exec::compact_tool_context_output(&calls[1], &executions[1]);
    assert!(context.contains("verify: python py_compile FAILED"));
    let ui = exec::summarize_tool_result(&calls[1], &executions[1]);
    assert!(ui.contains("edited second.py (1 replacement) · verify: python py_compile FAILED"));
}

#[test]
fn clean_edits_get_a_passing_verify_line() {
    let workspace = temp_workspace();
    fs::write(workspace.join("tool.py"), "x = 1\n").unwrap();
    let tools = ToolRuntime::new(&workspace).unwrap();
    let backend = super::DirectCodexBackend::new(&workspace).unwrap();

    let calls = vec![edit_call("call_1", "tool.py", "x = 1\n", "x = 2\n")];
    let mut state = types::ToolLoopState::default();
    let executions = backend
        .execute_turn_tool_calls(
            &tools,
            &calls,
            &mut state,
            crate::harness::HarnessPolicy::for_user_prompt("edit the file"),
            types::ToolLoopPolicy::mutation_allowed(),
            &mut |_| Ok(()),
        )
        .unwrap();

    assert!(
        executions[0]
            .output
            .contains("verify: python py_compile ok")
    );
}

#[test]
fn mutating_calls_are_barriers_between_read_batches() {
    let workspace = temp_workspace();
    fs::write(workspace.join("a.txt"), "before\n").unwrap();
    let tools = ToolRuntime::new(&workspace).unwrap();
    let backend = super::DirectCodexBackend::new(&workspace).unwrap();

    let calls = vec![
        read_call("call_1", "a.txt"),
        types::ToolCall {
            name: "file_edit".to_string(),
            call_id: "call_2".to_string(),
            arguments: r#"{"path":"a.txt","oldString":"before\n","newString":"after\n"}"#
                .to_string(),
            reasoning_content: None,
        },
        read_call("call_3", "a.txt"),
    ];

    let mut state = types::ToolLoopState::default();
    let executions = backend
        .execute_turn_tool_calls(
            &tools,
            &calls,
            &mut state,
            crate::harness::HarnessPolicy::for_user_prompt("edit the file"),
            types::ToolLoopPolicy::mutation_allowed(),
            &mut |_| Ok(()),
        )
        .unwrap();

    // Serial semantics hold around the barrier: the first read sees the old
    // content, the read after the edit sees the new content.
    assert!(executions[0].output.contains("before"));
    assert!(!executions[1].failed);
    assert!(executions[2].output.contains("after"));
}

#[test]
fn cancellation_between_serial_calls_stops_the_batch() {
    let workspace = temp_workspace();
    fs::write(workspace.join("first.txt"), "one\n").unwrap();
    fs::write(workspace.join("second.txt"), "two\n").unwrap();
    let tools = ToolRuntime::new(&workspace).unwrap();
    let backend = super::DirectCodexBackend::new(&workspace).unwrap();
    let cancel = tools.cancel_token().clone();

    // file_edit calls are serial barriers; cancelling after the first
    // completes must keep the second from ever executing.
    let calls = vec![
        edit_call("call_1", "first.txt", "one\n", "one edited\n"),
        edit_call("call_2", "second.txt", "two\n", "two edited\n"),
    ];

    let mut state = types::ToolLoopState::default();
    let error = backend
        .execute_turn_tool_calls(
            &tools,
            &calls,
            &mut state,
            crate::harness::HarnessPolicy::for_user_prompt("edit the files"),
            types::ToolLoopPolicy::mutation_allowed(),
            &mut |event| {
                if matches!(event, types::ModelStreamEvent::ToolResult { .. }) {
                    cancel.cancel();
                }
                Ok(())
            },
        )
        .unwrap_err();

    assert!(crate::cancel::error_is_cancellation(&error), "{error}");
    // The first call completed before cancellation; the second never ran.
    assert_eq!(
        fs::read_to_string(workspace.join("first.txt")).unwrap(),
        "one edited\n"
    );
    assert_eq!(
        fs::read_to_string(workspace.join("second.txt")).unwrap(),
        "two\n"
    );
}

#[test]
fn cancellation_mid_parallel_batch_abandons_stragglers_as_cancelled() {
    let workspace = temp_workspace();
    fs::write(workspace.join("a.txt"), "alpha\n").unwrap();
    fs::write(workspace.join("b.txt"), "beta\n").unwrap();
    let tools = ToolRuntime::new(&workspace).unwrap();
    let backend = super::DirectCodexBackend::new(&workspace).unwrap();
    let cancel = tools.cancel_token().clone();

    // Both reads run in one parallel batch; cancelling on the first
    // collected result forces the collector to abandon the other slot.
    let calls = vec![read_call("call_a", "a.txt"), read_call("call_b", "b.txt")];

    let mut results = Vec::new();
    let mut state = types::ToolLoopState::default();
    let error = backend
        .execute_turn_tool_calls(
            &tools,
            &calls,
            &mut state,
            crate::harness::HarnessPolicy::for_user_prompt("read the files"),
            types::ToolLoopPolicy::mutation_allowed(),
            &mut |event| {
                if let types::ModelStreamEvent::ToolResult { output, .. } = event {
                    cancel.cancel();
                    results.push(output);
                }
                Ok(())
            },
        )
        .unwrap_err();

    assert!(crate::cancel::error_is_cancellation(&error), "{error}");
    // Every slot resolved before the bail: one real result, one abandoned
    // straggler marked cancelled.
    assert_eq!(results.len(), 2, "{results:?}");
    assert!(
        results.iter().any(|output| output.contains("cancelled")),
        "{results:?}"
    );
}

#[test]
fn pre_cancelled_token_stops_tool_batches_before_any_call() {
    let workspace = temp_workspace();
    fs::write(workspace.join("a.txt"), "alpha\n").unwrap();
    let tools = ToolRuntime::new(&workspace).unwrap();
    let backend = super::DirectCodexBackend::new(&workspace).unwrap();
    tools.cancel_token().cancel();

    let calls = vec![edit_call("call_1", "a.txt", "alpha\n", "changed\n")];
    let mut state = types::ToolLoopState::default();
    let error = backend
        .execute_turn_tool_calls(
            &tools,
            &calls,
            &mut state,
            crate::harness::HarnessPolicy::for_user_prompt("edit the file"),
            types::ToolLoopPolicy::mutation_allowed(),
            &mut |_| Ok(()),
        )
        .unwrap_err();

    assert!(crate::cancel::error_is_cancellation(&error), "{error}");
    assert_eq!(
        fs::read_to_string(workspace.join("a.txt")).unwrap(),
        "alpha\n"
    );
}

#[test]
fn sleep_with_cancel_exits_promptly_when_cancelled() {
    let cancel = crate::cancel::CancelToken::new();
    let canceller = cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(50));
        canceller.cancel();
    });

    let started = std::time::Instant::now();
    let error = super::sleep_with_cancel(std::time::Duration::from_secs(30), &cancel).unwrap_err();

    assert!(crate::cancel::error_is_cancellation(&error), "{error}");
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
}

#[test]
fn sleep_with_cancel_completes_normally_without_cancellation() {
    let cancel = crate::cancel::CancelToken::default();
    super::sleep_with_cancel(std::time::Duration::from_millis(10), &cancel).unwrap();
}

/// A stream body that yields one SSE chunk, then stalls: the shape of a
/// silent model connection that only the pump + token can interrupt.
struct StallingSseBody {
    emitted: bool,
    stall: std::time::Duration,
}

impl std::io::Read for StallingSseBody {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if !self.emitted {
            self.emitted = true;
            let chunk: &[u8] = b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n";
            buffer[..chunk.len()].copy_from_slice(chunk);
            return Ok(chunk.len());
        }
        std::thread::sleep(self.stall);
        Ok(0)
    }
}

#[test]
fn stalled_chat_sse_stream_cancels_via_the_pump() {
    let cancel = crate::cancel::CancelToken::new();
    let canceller = cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(120));
        canceller.cancel();
    });

    let started = std::time::Instant::now();
    let mut deltas = Vec::new();
    let error = wire::read_chat_completions_sse_response(
        StallingSseBody {
            emitted: false,
            stall: std::time::Duration::from_secs(10),
        },
        &cancel,
        &mut |event| {
            if let types::ModelStreamEvent::Delta(delta) = event {
                deltas.push(delta);
            }
            Ok(())
        },
    )
    .unwrap_err();

    assert!(crate::cancel::error_is_cancellation(&error), "{error}");
    // Pump + 100ms recv_timeout: cancellation lands promptly, not after the
    // stall (10s) or a network timeout.
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
    // Data streamed before the stall was still delivered.
    assert_eq!(deltas, vec!["hi".to_string()]);
}

#[test]
fn stalled_codex_sse_stream_cancels_via_the_pump() {
    let cancel = crate::cancel::CancelToken::new();
    cancel.cancel();

    let started = std::time::Instant::now();
    let error = wire::read_sse_response(
        StallingSseBody {
            emitted: true,
            stall: std::time::Duration::from_secs(10),
        },
        &cancel,
        &mut |_| Ok(()),
    )
    .unwrap_err();

    assert!(crate::cancel::error_is_cancellation(&error), "{error}");
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
}

#[test]
fn complete_sse_streams_parse_identically_through_the_pump() {
    let stream = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\
data: {\"type\":\"response.output_text.delta\",\"delta\":\" medusa\"}\n\
data: {\"type\":\"response.completed\"}\n";

    let mut text = String::new();
    let outcome = wire::read_sse_response(
        std::io::Cursor::new(stream.as_bytes().to_vec()),
        &crate::cancel::CancelToken::default(),
        &mut |event| {
            if let types::ModelStreamEvent::Delta(delta) = event {
                text.push_str(&delta);
            }
            Ok(())
        },
    )
    .unwrap();

    assert_eq!(text, "hello medusa");
    assert!(outcome.tool_calls.is_empty());
}

#[test]
fn terminal_ui_summary_includes_output_tail_and_context_strips_ansi() {
    let call = types::ToolCall {
        name: "terminal_exec".to_string(),
        call_id: "call_test".to_string(),
        arguments: r#"{"command":"cargo test"}"#.to_string(),
        reasoning_content: None,
    };
    let execution = types::ToolExecution {
        output: "exit: 0\nstdout:\nrunning 3 tests\n\u{1b}[32mtest a ... ok\u{1b}[0m\ntest result: ok. 3 passed; 0 failed\n".to_string(),
        failed: false,
    };

    let ui = exec::summarize_tool_result(&call, &execution);
    assert!(ui.starts_with("exit: 0 • test result: ok. 3 passed; 0 failed"));
    // Tail lines keep ANSI for the transcript renderer.
    assert!(ui.contains("\u{1b}[32mtest a ... ok\u{1b}[0m"));

    let context = exec::compact_tool_context_output(&call, &execution);
    assert!(
        !context.contains('\u{1b}'),
        "model context must be ANSI-free"
    );
    assert!(context.contains("3 passed"));
}

#[test]
fn sandbox_notes_survive_both_summary_paths() {
    let call = types::ToolCall {
        name: "terminal_exec".to_string(),
        call_id: "call_test".to_string(),
        arguments: r#"{"command":"touch /etc/hosts"}"#.to_string(),
        reasoning_content: None,
    };
    // Shape produced by execute_terminal_exec for a failed sandboxed run:
    // notes live between the exit line and the stdout marker.
    let execution = types::ToolExecution {
        output: "exit: 1\nsandbox: ran under macOS Seatbelt (writes confined to workspace/temp; network denied unless enabled)\nhint: if the sandbox caused this failure, retry with \"sandbox\": false and explain why; the user must approve every unsandboxed run\nstdout: <empty>\nstderr:\ntouch: /etc/hosts: Operation not permitted\n".to_string(),
        failed: true,
    };

    let ui = exec::summarize_tool_result(&call, &execution);
    assert!(ui.contains("sandbox: ran under macOS Seatbelt"), "{ui}");

    let context = exec::compact_tool_context_output(&call, &execution);
    assert!(
        context.contains("sandbox: ran under macOS Seatbelt"),
        "{context}"
    );
    assert!(
        context.contains("retry with \"sandbox\": false"),
        "{context}"
    );
    // The real command output still comes through.
    assert!(context.contains("Operation not permitted"), "{context}");
}

#[test]
fn unsandboxed_terminal_calls_are_tagged_in_the_call_summary() {
    let call = |arguments: &str| types::ToolCall {
        name: "terminal_exec".to_string(),
        call_id: "call_test".to_string(),
        arguments: arguments.to_string(),
        reasoning_content: None,
    };

    assert_eq!(
        exec::summarize_tool_call(&call(r#"{"command":"cargo fetch","sandbox":false}"#)),
        "$ cargo fetch · unsandboxed"
    );
    assert_eq!(
        exec::summarize_tool_call(&call(r#"{"command":"cargo fetch"}"#)),
        "$ cargo fetch"
    );
}

#[test]
fn terminal_exec_sandbox_false_requires_an_approver() {
    let workspace = temp_workspace();
    crate::permissions::PermissionPolicy::write_mode(
        &workspace,
        crate::permissions::PermissionMode::Guarded,
    )
    .unwrap();
    let tools = ToolRuntime::new(&workspace).unwrap();
    let call = types::ToolCall {
        name: "terminal_exec".to_string(),
        call_id: "call_test".to_string(),
        arguments: r#"{"command":"printf hi","sandbox":false}"#.to_string(),
        reasoning_content: None,
    };

    let execution = exec::execute_tool_call(
        &tools,
        &call,
        &types::ToolLoopState::default(),
        types::ToolLoopPolicy::mutation_allowed(),
    );

    assert!(execution.failed);
    assert!(
        execution.output.contains("requires approval"),
        "{}",
        execution.output
    );
}

#[cfg(target_os = "macos")]
#[test]
fn live_sandboxed_failure_output_carries_note_and_escalation_hint() {
    use crate::sandbox::{SandboxAvailability, SandboxPolicy, sandbox_availability};
    if *sandbox_availability() != SandboxAvailability::Available {
        eprintln!("skipping: sandbox-exec unavailable on this machine");
        return;
    }

    let workspace = temp_workspace();
    let tools = ToolRuntime::new(&workspace)
        .unwrap()
        .with_sandbox(SandboxPolicy::new(true, false, Vec::new()));
    // HOME is never a default writable root; the write is denied so nothing
    // is created.
    let call = types::ToolCall {
        name: "terminal_exec".to_string(),
        call_id: "call_test".to_string(),
        arguments: r#"{"command":"touch \"$HOME/medusa-exec-escape-test.txt\""}"#.to_string(),
        reasoning_content: None,
    };

    let execution = exec::execute_tool_call(
        &tools,
        &call,
        &types::ToolLoopState::default(),
        types::ToolLoopPolicy::mutation_allowed(),
    );
    let created = std::path::PathBuf::from(std::env::var_os("HOME").expect("HOME set"))
        .join("medusa-exec-escape-test.txt");
    let escaped = created.exists();
    let _ = fs::remove_file(&created);

    assert!(!escaped, "sandboxed command wrote outside its roots");
    assert!(execution.failed);
    assert!(
        execution
            .output
            .contains("sandbox: ran under macOS Seatbelt"),
        "{}",
        execution.output
    );
    assert!(
        execution.output.contains("retry with \"sandbox\": false"),
        "{}",
        execution.output
    );
}

#[test]
fn file_edit_ui_summary_includes_diff_but_model_context_does_not() {
    let call = types::ToolCall {
        name: "file_edit".to_string(),
        call_id: "call_test".to_string(),
        arguments: r#"{"path":"src/lib.rs","oldString":"fn old() {}\nshared line","newString":"fn new() {}\nshared line"}"#.to_string(),
        reasoning_content: None,
    };
    let execution = types::ToolExecution {
        output: "edited files:\nsrc/lib.rs\nreplacements: 1".to_string(),
        failed: false,
    };

    let ui = exec::summarize_tool_result(&call, &execution);
    assert!(ui.starts_with("edited src/lib.rs (1 replacement)"));
    assert!(ui.contains("- fn old() {}"));
    assert!(ui.contains("+ fn new() {}"));

    let context = exec::compact_tool_context_output(&call, &execution);
    assert_eq!(context, "edited src/lib.rs (1 replacement)");
}

#[test]
fn file_patch_ui_summary_includes_diff_body() {
    let call = types::ToolCall {
        name: "file_patch".to_string(),
        call_id: "call_test".to_string(),
        arguments: serde_json::json!({
            "diff": "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,2 +1,2 @@\n-old line\n+new line\n context"
        })
        .to_string(),
        reasoning_content: None,
    };
    let execution = types::ToolExecution {
        output: "patched files:\nsrc/lib.rs".to_string(),
        failed: false,
    };

    let ui = exec::summarize_tool_result(&call, &execution);
    assert!(ui.starts_with("patched src/lib.rs"));
    assert!(ui.contains("- old line"));
    assert!(ui.contains("+ new line"));

    let context = exec::compact_tool_context_output(&call, &execution);
    assert_eq!(context, "patched src/lib.rs");
}

#[test]
fn file_read_tool_executes() {
    let workspace = temp_workspace();
    fs::write(workspace.join("hello.txt"), "alpha\nbeta\n").unwrap();
    let tools = ToolRuntime::new(&workspace).unwrap();
    let call = types::ToolCall {
        name: "file_read".to_string(),
        call_id: "call_test".to_string(),
        arguments: r#"{"paths":["hello.txt"],"start_line":2,"end_line":2}"#.to_string(),
        reasoning_content: None,
    };

    let execution = exec::execute_tool_call(
        &tools,
        &call,
        &types::ToolLoopState::default(),
        types::ToolLoopPolicy::mutation_allowed(),
    );

    assert!(!execution.failed);
    assert!(execution.output.contains("hello.txt:2-2"));
    assert!(execution.output.contains("beta"));
}

#[test]
fn file_edit_tool_executes_with_opencode_arguments() {
    let workspace = temp_workspace();
    fs::write(workspace.join("hello.txt"), "alpha\nold\n").unwrap();
    let tools = ToolRuntime::new(&workspace).unwrap();
    let call = types::ToolCall {
        name: "file_edit".to_string(),
        call_id: "call_test".to_string(),
        arguments: r#"{"path":"hello.txt","oldString":"old\n","newString":"new\n"}"#.to_string(),
        reasoning_content: None,
    };

    let execution = exec::execute_tool_call(
        &tools,
        &call,
        &types::ToolLoopState::default(),
        types::ToolLoopPolicy::mutation_allowed(),
    );

    assert!(!execution.failed);
    assert!(execution.output.contains("edited files:"));
    assert_eq!(
        fs::read_to_string(workspace.join("hello.txt")).unwrap(),
        "alpha\nnew\n"
    );
}

#[test]
fn read_only_tool_policy_blocks_mutation_tools() {
    let workspace = temp_workspace();
    fs::write(workspace.join("hello.txt"), "alpha\nold\n").unwrap();
    let tools = ToolRuntime::new(&workspace).unwrap();
    let call = types::ToolCall {
        name: "file_edit".to_string(),
        call_id: "call_test".to_string(),
        arguments: r#"{"path":"hello.txt","oldString":"old\n","newString":"new\n"}"#.to_string(),
        reasoning_content: None,
    };

    let execution = exec::execute_tool_call(
        &tools,
        &call,
        &types::ToolLoopState::default(),
        types::ToolLoopPolicy::read_only(),
    );

    assert!(execution.failed);
    assert!(execution.output.contains("read-only workflow subagent"));
    assert_eq!(
        fs::read_to_string(workspace.join("hello.txt")).unwrap(),
        "alpha\nold\n"
    );
}

#[test]
fn terminal_exec_clears_patch_recovery_state() {
    let mut state = types::ToolLoopState {
        patch_requires_context: true,
    };
    let call = types::ToolCall {
        name: "terminal_exec".to_string(),
        call_id: "call_test".to_string(),
        arguments: "{}".to_string(),
        reasoning_content: None,
    };
    let execution = types::ToolExecution {
        output: String::new(),
        failed: false,
    };

    exec::update_tool_loop_state(&mut state, &call, &execution);

    assert!(!state.patch_requires_context);
}

#[test]
fn structured_read_clears_patch_recovery_state() {
    let mut state = types::ToolLoopState {
        patch_requires_context: true,
    };
    let call = types::ToolCall {
        name: "file_read".to_string(),
        call_id: "call_test".to_string(),
        arguments: "{}".to_string(),
        reasoning_content: None,
    };
    let execution = types::ToolExecution {
        output: String::new(),
        failed: false,
    };

    exec::update_tool_loop_state(&mut state, &call, &execution);

    assert!(!state.patch_requires_context);
}

#[test]
fn pre_tool_hook_can_block_tool_execution() {
    let workspace = temp_workspace();
    fs::create_dir_all(workspace.join(".medusa")).unwrap();
    fs::write(
        workspace.join(".medusa/hooks.json"),
        r#"{"hooks":{"pre_tool":[{"command":"echo blocked >&2; exit 9","fail_on_error":true}]}}"#,
    )
    .unwrap();
    let tools = ToolRuntime::new(&workspace).unwrap();
    let call = types::ToolCall {
        name: "terminal_exec".to_string(),
        call_id: "call_test".to_string(),
        arguments: r#"{"command":"printf should-not-run"}"#.to_string(),
        reasoning_content: None,
    };

    let execution = exec::execute_tool_call_with_hooks(
        &tools,
        &call,
        &types::ToolLoopState::default(),
        HarnessPolicy::for_user_prompt("fix tests"),
        types::ToolLoopPolicy::mutation_allowed(),
    );

    assert!(execution.failed);
    assert!(execution.output.contains("pre_tool hook blocked"));
    assert!(execution.output.contains("blocked"));
}

fn mcp_call(name: &str, arguments: &str, call_id: &str) -> types::ToolCall {
    types::ToolCall {
        name: name.to_string(),
        call_id: call_id.to_string(),
        arguments: arguments.to_string(),
        reasoning_content: None,
    }
}

#[test]
fn execute_tool_call_dispatches_mcp_tools_through_the_registry() {
    let workspace = crate::mcp::tests::write_fake_server_workspace("fake", &[], false);
    let registry = crate::mcp::McpRegistry::load(&workspace).unwrap();
    let tools = ToolRuntime::new(&workspace).unwrap().with_mcp(registry);
    // Open mode (default) trusts the config: this approves the server launch
    // and discovers its tools so the namespaced dispatch resolves.
    tools.mcp_tool_schemas(true);
    let state = types::ToolLoopState::default();

    // Happy path: joined text content, not failed.
    let execution = exec::execute_tool_call(
        &tools,
        &mcp_call("mcp_fake_echo", r#"{"text":"hi"}"#, "call_1"),
        &state,
        types::ToolLoopPolicy::mutation_allowed(),
    );
    assert!(!execution.failed, "{}", execution.output);
    assert_eq!(execution.output, "echo: hi");

    // isError from the server maps to a failed execution.
    let execution = exec::execute_tool_call(
        &tools,
        &mcp_call("mcp_fake_echo", r#"{"error":true}"#, "call_2"),
        &state,
        types::ToolLoopPolicy::mutation_allowed(),
    );
    assert!(execution.failed);
    assert!(execution.output.contains("boom"), "{}", execution.output);
    assert!(
        execution.output.contains("fake:echo"),
        "{}",
        execution.output
    );

    // Read-only turns reject MCP tools whose server lacks "readOnly": true.
    let execution = exec::execute_tool_call(
        &tools,
        &mcp_call("mcp_fake_echo", r#"{"text":"hi"}"#, "call_3"),
        &state,
        types::ToolLoopPolicy::read_only(),
    );
    assert!(execution.failed);
    assert!(
        execution.output.contains("read-only"),
        "{}",
        execution.output
    );
}

#[test]
fn read_only_turns_omit_mcp_schemas_and_read_only_servers_survive() {
    let workspace = crate::mcp::tests::write_fake_server_workspace("fake", &[], false);
    let registry = crate::mcp::McpRegistry::load(&workspace).unwrap();
    let tools = ToolRuntime::new(&workspace).unwrap().with_mcp(registry);

    // This mirrors the turn loop's gate: allow_mutation() selects the flag.
    assert!(tools.mcp_tool_schemas(false).is_empty());
    assert_eq!(tools.mcp_tool_schemas(true).len(), 1);

    // A runtime without a registry advertises nothing.
    let bare = ToolRuntime::new(&workspace).unwrap();
    assert!(bare.mcp_tool_schemas(true).is_empty());
}

#[test]
fn mcp_calls_are_serial_barriers_not_read_only() {
    assert!(!exec::tool_call_is_read_only("mcp_fake_echo"));
    assert!(!types::is_mutation_tool("mcp_fake_echo"));
}

#[test]
fn mcp_tool_calls_summarize_and_compact_as_dynamic_tools() {
    let call = mcp_call("mcp_fake_echo", r#"{"text":"hi"}"#, "call_1");
    assert_eq!(
        exec::summarize_tool_call(&call),
        r#"mcp_fake_echo {"text":"hi"}"#
    );
    assert_eq!(exec::display_tool_name("mcp_fake_echo"), "mcp_fake_echo");

    // Model-context output is capped at 8000 chars for MCP results.
    let execution = types::ToolExecution {
        failed: false,
        output: "x".repeat(9_000),
    };
    let context = exec::compact_tool_context_output(&call, &execution);
    assert_eq!(context.chars().count(), 8_003, "8000 chars plus ellipsis");

    // Transcript summaries use the compact default.
    let summary = exec::summarize_tool_result(&call, &execution);
    assert!(summary.chars().count() <= 503, "{}", summary.len());
}

#[test]
#[ignore = "uses the local Codex OAuth cache and calls the live Codex backend"]
fn live_direct_oauth_smoke() {
    let cwd = std::env::current_dir().unwrap();
    let backend = super::DirectCodexBackend::new(&cwd).unwrap();
    let tools = ToolRuntime::new(&cwd).unwrap();

    let result = backend
        .chat(
            "Reply with exactly this text and nothing else: medusa-live-ok",
            tools,
        )
        .unwrap();

    assert_eq!(result.response.trim(), "medusa-live-ok");
}

#[test]
#[ignore = "uses the local Codex OAuth cache and calls the live Codex backend"]
fn live_tool_loop_smoke() {
    let cwd = std::env::current_dir().unwrap();
    let backend = super::DirectCodexBackend::new(&cwd).unwrap();
    let tools = ToolRuntime::new(&cwd).unwrap();

    let result = backend
        .chat(
            "Use terminal_exec to run `pwd`, then answer exactly: tool-loop-ok",
            tools,
        )
        .unwrap();

    assert_eq!(result.response.trim(), "tool-loop-ok");
}

fn temp_workspace() -> std::path::PathBuf {
    static TEMP_COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let index = TEMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let pid = std::process::id();
    let path = std::env::temp_dir().join(format!("medusa-model-test-{pid}-{suffix}-{index}"));
    fs::create_dir_all(&path).unwrap();
    path.canonicalize().unwrap()
}
