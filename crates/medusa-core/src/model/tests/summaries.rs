use super::*;

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
fn read_only_tool_policy_blocks_mutating_terminal_commands() {
    let workspace = temp_workspace();
    let tools = ToolRuntime::new(&workspace).unwrap();
    let call = types::ToolCall {
        name: "terminal_exec".to_string(),
        call_id: "call_test".to_string(),
        arguments: r#"{"command":"touch should-not-exist"}"#.to_string(),
        reasoning_content: None,
    };

    let execution = exec::execute_tool_call(
        &tools,
        &call,
        &types::ToolLoopState::default(),
        types::ToolLoopPolicy::read_only(),
    );

    assert!(execution.failed);
    assert!(execution.output.contains("restricted to read-only"));
    assert!(!workspace.join("should-not-exist").exists());
}

#[test]
fn terminal_exec_clears_patch_recovery_state() {
    let mut state = types::ToolLoopState {
        patch_requires_context: true,
        ..types::ToolLoopState::default()
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
        ..types::ToolLoopState::default()
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
