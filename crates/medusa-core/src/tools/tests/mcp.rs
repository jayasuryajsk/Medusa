use super::*;

/// Fake-MCP-backed runtime with the server's launch pre-approved (so these
/// tests exercise the per-call gate, not the launch gate — that is covered
/// separately) and its tools discovered so the namespaced lookup resolves.
fn mcp_runtime(mode: crate::permissions::PermissionMode, read_only: bool) -> ToolRuntime {
    mcp_runtime_env(mode, read_only, &[])
}

fn mcp_runtime_env(
    mode: crate::permissions::PermissionMode,
    read_only: bool,
    env: &[(&str, &str)],
) -> ToolRuntime {
    let workspace = crate::mcp::tests::write_fake_server_workspace("fake", env, read_only);
    crate::permissions::PermissionPolicy::write_mode(&workspace, mode).unwrap();
    let registry = McpRegistry::load(&workspace).unwrap();
    registry.mark_server_launch_approved("fake");
    registry.tool_schemas(true, &CancelToken::new());
    ToolRuntime::new(&workspace).unwrap().with_mcp(registry)
}

#[test]
fn mcp_call_runs_openly_in_open_mode_without_prompting() {
    let runtime = mcp_runtime(crate::permissions::PermissionMode::Open, false)
        .with_approval_handler(Arc::new(|request: ApprovalRequest| {
            panic!("open mode must not prompt for MCP: {request:?}")
        }));

    let outcome = runtime
        .mcp_call("mcp_fake_echo", &serde_json::json!({"text": "hi"}))
        .unwrap();

    assert_eq!(outcome.text, "echo: hi");
    assert!(!outcome.is_error);
}

#[test]
fn mcp_call_in_ask_mode_without_handler_is_auto_denied() {
    let runtime = mcp_runtime(crate::permissions::PermissionMode::Ask, false);

    let error = runtime
        .mcp_call("mcp_fake_echo", &serde_json::json!({"text": "hi"}))
        .unwrap_err();

    assert!(error.to_string().contains("auto-denied"), "{error}");
}

#[test]
fn mcp_allow_once_authorizes_exactly_one_call() {
    // Finding 4: "allow once" must not persist. Two calls to the same tool
    // prompt twice — the pre-fix code unlocked the whole server after one.
    let prompts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let prompts_in_handler = Arc::clone(&prompts);
    let runtime = mcp_runtime(crate::permissions::PermissionMode::Ask, false)
        .with_approval_handler(Arc::new(move |request: ApprovalRequest| {
            assert_eq!(request.tool, ApprovalTool::McpTool);
            assert!(
                request
                    .command
                    .as_deref()
                    .unwrap_or_default()
                    .starts_with("fake:echo"),
                "{request:?}"
            );
            prompts_in_handler.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            ApprovalDecision::AllowOnce
        }));

    let first = runtime
        .mcp_call("mcp_fake_echo", &serde_json::json!({"text": "one"}))
        .unwrap();
    let second = runtime
        .mcp_call("mcp_fake_echo", &serde_json::json!({"text": "two"}))
        .unwrap();

    assert_eq!(first.text, "echo: one");
    assert_eq!(second.text, "echo: two");
    assert_eq!(
        prompts.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "allow-once must prompt on every call, not unlock the server"
    );
}

#[test]
fn mcp_always_allow_is_scoped_to_the_single_tool() {
    // Finding 4 (core): always-allowing one tool must NOT unlock the
    // server's other (possibly mutating) tools. Approving `echo` must
    // never approve `extra` — the `db_query` → `db_drop_table` hole.
    let prompts = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let prompts_in_handler = Arc::clone(&prompts);
    let runtime = mcp_runtime_env(
        crate::permissions::PermissionMode::Ask,
        false,
        &[("FAKE_PAGINATE", "1")],
    )
    .with_approval_handler(Arc::new(move |request: ApprovalRequest| {
        assert_eq!(request.tool, ApprovalTool::McpTool);
        prompts_in_handler
            .lock()
            .unwrap()
            .push(request.command.clone().unwrap_or_default());
        ApprovalDecision::AlwaysAllow
    }));

    // echo: first call prompts (always-allow), the second is silent.
    runtime
        .mcp_call("mcp_fake_echo", &serde_json::json!({"text": "a"}))
        .unwrap();
    runtime
        .mcp_call("mcp_fake_echo", &serde_json::json!({"text": "b"}))
        .unwrap();
    // extra: a different tool on the same server still prompts.
    runtime
        .mcp_call("mcp_fake_extra", &serde_json::json!({"text": "c"}))
        .unwrap();

    let commands = prompts.lock().unwrap();
    assert_eq!(
        commands.len(),
        2,
        "echo prompts once, extra prompts once: {commands:?}"
    );
    assert!(commands[0].starts_with("fake:echo"), "{commands:?}");
    assert!(
        commands[1].starts_with("fake:extra"),
        "approving echo must not unlock extra: {commands:?}"
    );
}

#[test]
fn mcp_call_denial_blocks_and_reprompts_each_call() {
    let prompts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let prompts_in_handler = Arc::clone(&prompts);
    let runtime = mcp_runtime(crate::permissions::PermissionMode::Ask, false)
        .with_approval_handler(Arc::new(move |_request: ApprovalRequest| {
            prompts_in_handler.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            ApprovalDecision::Deny
        }));

    for _ in 0..2 {
        let error = runtime
            .mcp_call("mcp_fake_echo", &serde_json::json!({"text": "no"}))
            .unwrap_err();
        assert!(error.to_string().contains("denied by user"), "{error}");
    }

    assert_eq!(
        prompts.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "a denial must not unlock the server"
    );
}

#[test]
fn guarded_launch_denied_never_spawns_and_is_remembered() {
    // Finding 14: schema build in a confined mode must prompt to launch
    // the server (which runs its command); a denial spawns nothing and is
    // remembered so it doesn't re-prompt every turn.
    let workspace = crate::mcp::tests::write_fake_server_workspace("fake", &[], false);
    crate::permissions::PermissionPolicy::write_mode(
        &workspace,
        crate::permissions::PermissionMode::Guarded,
    )
    .unwrap();
    let registry = McpRegistry::load(&workspace).unwrap();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let calls_in_handler = Arc::clone(&calls);
    let runtime = ToolRuntime::new(&workspace)
        .unwrap()
        .with_mcp(registry.clone())
        .with_approval_handler(Arc::new(move |request: ApprovalRequest| {
            assert_eq!(request.tool, ApprovalTool::McpServerLaunch);
            assert!(
                request
                    .command
                    .as_deref()
                    .unwrap_or_default()
                    .contains("launch MCP server `fake`"),
                "{request:?}"
            );
            calls_in_handler.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            ApprovalDecision::Deny
        }));

    assert!(
        runtime.mcp_tool_schemas(true).is_empty(),
        "a denied launch advertises no tools"
    );
    assert_eq!(
        registry.statuses()[0].state,
        crate::mcp::McpServerStateLabel::Idle,
        "a denied launch must not spawn the process"
    );
    // A second turn's schema build does not re-prompt.
    assert!(runtime.mcp_tool_schemas(true).is_empty());
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the launch decision is remembered for the session"
    );
}

#[test]
fn guarded_launch_approval_spawns_once_then_calls_gate_separately() {
    // Finding 14: approving the launch spawns the server and advertises
    // its tools; the approval is once per session (no re-prompt).
    let workspace = crate::mcp::tests::write_fake_server_workspace("fake", &[], false);
    crate::permissions::PermissionPolicy::write_mode(
        &workspace,
        crate::permissions::PermissionMode::Guarded,
    )
    .unwrap();
    let registry = McpRegistry::load(&workspace).unwrap();
    let launches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let launches_in_handler = Arc::clone(&launches);
    let runtime = ToolRuntime::new(&workspace)
        .unwrap()
        .with_mcp(registry.clone())
        .with_approval_handler(Arc::new(move |request: ApprovalRequest| {
            assert_eq!(request.tool, ApprovalTool::McpServerLaunch);
            launches_in_handler.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            ApprovalDecision::AllowOnce
        }));

    let schemas = runtime.mcp_tool_schemas(true);
    assert_eq!(schemas.len(), 1);
    assert_eq!(schemas[0]["name"], "mcp_fake_echo");
    assert_eq!(
        registry.statuses()[0].state,
        crate::mcp::McpServerStateLabel::Ready
    );

    runtime.mcp_tool_schemas(true);
    assert_eq!(
        launches.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "launch approval is once per session"
    );
}

#[test]
fn readonly_mode_only_allows_servers_marked_read_only() {
    let denied = mcp_runtime(crate::permissions::PermissionMode::Readonly, false);
    let error = denied
        .mcp_call("mcp_fake_echo", &serde_json::json!({"text": "hi"}))
        .unwrap_err();
    assert!(
        error.to_string().contains("readonly permissions"),
        "{error}"
    );

    let allowed = mcp_runtime(crate::permissions::PermissionMode::Readonly, true);
    let outcome = allowed
        .mcp_call("mcp_fake_echo", &serde_json::json!({"text": "ok"}))
        .unwrap();
    assert_eq!(outcome.text, "echo: ok");
}

#[test]
fn mcp_call_without_registry_or_unknown_tool_fails_clearly() {
    let runtime = ToolRuntime::new(temp_workspace()).unwrap();
    let error = runtime
        .mcp_call("mcp_fake_echo", &serde_json::json!({}))
        .unwrap_err();
    assert!(error.to_string().contains("no MCP registry"), "{error}");

    let runtime = runtime.with_mcp(McpRegistry::empty());
    let error = runtime
        .mcp_call("mcp_missing_tool", &serde_json::json!({}))
        .unwrap_err();
    assert!(error.to_string().contains("unknown MCP tool"), "{error}");
}
