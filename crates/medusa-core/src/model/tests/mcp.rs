use super::*;

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
    crate::permissions::PermissionPolicy::write_mode(
        &workspace,
        crate::permissions::PermissionMode::Open,
    )
    .unwrap();
    let registry = crate::mcp::McpRegistry::load(&workspace).unwrap();
    let tools = ToolRuntime::new(&workspace).unwrap().with_mcp(registry);
    // Explicit open mode trusts the config: this approves the server launch
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
    crate::permissions::PermissionPolicy::write_mode(
        &workspace,
        crate::permissions::PermissionMode::Open,
    )
    .unwrap();
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
