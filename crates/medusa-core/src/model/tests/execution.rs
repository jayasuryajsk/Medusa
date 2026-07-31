use super::*;

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
