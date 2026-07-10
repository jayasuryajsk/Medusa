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
    let all_tools = schema::medusa_tools(true, true);
    let recovery_tools = schema::medusa_tools(false, false);

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
    let tools = schema::medusa_tools(true, true);
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
fn workflow_tool_only_offered_to_main_turns() {
    let main_tools = schema::medusa_tools(true, true);
    let subagent_tools = schema::medusa_tools(true, false);

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
    let tools = schema::chat_completion_tools(true, true);
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
fn mutation_tools_gated_by_policy_not_turn_mode() {
    let mutation_tools = schema::medusa_tools(true, true);
    let read_only_tools = schema::medusa_tools(false, false);

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
    );

    assert!(instructions.contains("Active Medusa skills"));
    assert!(instructions.contains("lead with findings"));
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
    assert!(executions[1].output.contains("verify: python py_compile FAILED"));
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

    assert!(executions[0].output.contains("verify: python py_compile ok"));
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
    assert!(!context.contains('\u{1b}'), "model context must be ANSI-free");
    assert!(context.contains("3 passed"));
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
