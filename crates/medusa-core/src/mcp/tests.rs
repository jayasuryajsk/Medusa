use super::*;
use std::sync::atomic::AtomicUsize;

/// Fake stdio MCP server: answers initialize/tools/list, and its tools'
/// behavior is driven by the call arguments (error, sleep, crash,
/// garbage-before-reply, wedge, ping_probe, count_file). Env vars select
/// handshake/pagination quirks. Uses an explicit readline loop so a tool
/// handler can synchronously read a nested reply (the ping probe).
const FAKE_SERVER: &str = r#"
import json, os, sys, time

def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

def readmsg():
    line = sys.stdin.readline()
    if line == "":
        return None
    line = line.strip()
    if not line:
        return {}
    return json.loads(line)

PAGINATE = os.environ.get("FAKE_PAGINATE") == "1"
BAD_VERSION = os.environ.get("FAKE_BAD_VERSION") == "1"

TOOLS = [
    {"name": "echo", "description": "Echo text back",
     "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}},
    {"name": "extra", "description": "Second tool", "inputSchema": {"type": "object"}},
]

while True:
    msg = readmsg()
    if msg is None:
        break
    if not msg:
        continue
    method = msg.get("method")
    mid = msg.get("id")
    if method == "initialize":
        version = "1999-01-01" if BAD_VERSION else msg["params"]["protocolVersion"]
        send({"jsonrpc": "2.0", "id": mid, "result": {
            "protocolVersion": version, "capabilities": {"tools": {}},
            "serverInfo": {"name": "fake", "version": "0"}}})
    elif method == "notifications/initialized":
        pass
    elif method == "tools/list":
        cursor = (msg.get("params") or {}).get("cursor")
        if PAGINATE and cursor is None:
            send({"jsonrpc": "2.0", "id": mid,
                  "result": {"tools": [TOOLS[0]], "nextCursor": "page2"}})
        elif PAGINATE:
            send({"jsonrpc": "2.0", "id": mid, "result": {"tools": [TOOLS[1]]}})
        else:
            send({"jsonrpc": "2.0", "id": mid, "result": {"tools": [TOOLS[0]]}})
    elif method == "tools/call":
        args = (msg.get("params") or {}).get("arguments") or {}
        # Observable side effect: append one line per invocation so a test can
        # prove the call ran exactly once (never a silent respawn+replay).
        if args.get("count_file"):
            with open(args["count_file"], "a") as fh:
                fh.write("x\n")
        if args.get("crash"):
            os._exit(1)
        if args.get("wedge"):
            # Never reply; drain stdin until EOF so the caller's request must
            # time out or cancel, then exit cleanly on shutdown.
            while readmsg() is not None:
                pass
            os._exit(0)
        if args.get("sleep"):
            time.sleep(float(args["sleep"]))
        if args.get("garbage"):
            sys.stdout.write("this is not json\n")
            sys.stdout.flush()
        if args.get("ping_probe"):
            # Send a server-initiated ping and report whether the client
            # answered with a success result (spec) rather than an error.
            send({"jsonrpc": "2.0", "id": "srv-ping", "method": "ping"})
            reply = None
            while True:
                nxt = readmsg()
                if nxt is None:
                    break
                if nxt.get("id") == "srv-ping":
                    reply = nxt
                    break
            ok = bool(reply) and ("result" in reply) and ("error" not in reply)
            send({"jsonrpc": "2.0", "id": mid, "result": {"content": [
                {"type": "text", "text": "ping_ok" if ok else "ping_bad"}]}})
        elif args.get("nontext"):
            send({"jsonrpc": "2.0", "id": mid, "result": {"content": [
                {"type": "image", "data": "xx", "mimeType": "image/png"},
                {"type": "text", "text": "with image"}]}})
        elif args.get("error"):
            send({"jsonrpc": "2.0", "id": mid, "result": {
                "content": [{"type": "text", "text": "boom"}], "isError": True}})
        else:
            send({"jsonrpc": "2.0", "id": mid, "result": {"content": [
                {"type": "text", "text": "echo: " + str(args.get("text", ""))}]}})
    elif mid is not None:
        send({"jsonrpc": "2.0", "id": mid,
              "error": {"code": -32601, "message": "unknown method"}})
"#;

pub(crate) fn temp_workspace() -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let unique = NEXT.fetch_add(1, Ordering::Relaxed);
    let path =
        std::env::temp_dir().join(format!("medusa-mcp-test-{}-{unique}", std::process::id()));
    fs::create_dir_all(&path).unwrap();
    path.canonicalize().unwrap()
}

/// Write the fake server plus a `.medusa/mcp.json` declaring it under
/// `server_name`, with extra env vars for behavior flags.
pub(crate) fn write_fake_server_workspace(
    server_name: &str,
    env: &[(&str, &str)],
    read_only: bool,
) -> PathBuf {
    let workspace = temp_workspace();
    let script = workspace.join("fake_mcp_server.py");
    fs::write(&script, FAKE_SERVER).unwrap();

    let env_json = env
        .iter()
        .map(|(key, value)| format!("\"{key}\": \"{value}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let config = format!(
        r#"{{"servers": {{"{server_name}": {{
                "command": "python3",
                "args": ["{}"],
                "env": {{ {env_json} }},
                "readOnly": {read_only}
            }}}}}}"#,
        script.display()
    );
    fs::create_dir_all(workspace.join(".medusa")).unwrap();
    fs::write(workspace.join(".medusa/mcp.json"), config).unwrap();
    workspace
}

fn call_timeout() -> Duration {
    Duration::from_secs(10)
}

fn no_cancel() -> CancelToken {
    CancelToken::new()
}

/// Load the registry and approve every server's launch, so transport-level
/// tests exercise the wire rather than the launch gate (which is covered
/// separately). Mirrors Open-mode trust of the workspace config.
fn loaded(workspace: impl Into<PathBuf>) -> Arc<McpRegistry> {
    let registry = McpRegistry::load(workspace).unwrap();
    registry.approve_all_launches();
    registry
}

#[test]
fn missing_config_yields_empty_registry() {
    let registry = McpRegistry::load(temp_workspace()).unwrap();

    assert!(registry.is_empty());
    assert!(registry.tool_schemas(true, &no_cancel()).is_empty());
    assert!(registry.statuses().is_empty());
}

#[test]
fn unapproved_launch_never_spawns_a_process() {
    // Finding 14: a configured server must not run until its launch is
    // approved. With no approval, tool_schemas/call_tool spawn nothing and
    // the server stays Idle.
    let workspace = write_fake_server_workspace("fake", &[], false);
    let registry = McpRegistry::load(&workspace).unwrap();

    assert!(
        registry.tool_schemas(true, &no_cancel()).is_empty(),
        "no launch approval → no tools advertised"
    );
    assert_eq!(registry.statuses()[0].state, McpServerStateLabel::Idle);

    let error = registry
        .call_tool(
            "fake",
            "echo",
            &json!({"text": "hi"}),
            call_timeout(),
            &no_cancel(),
        )
        .unwrap_err();
    assert!(
        error.to_string().contains("not approved to launch"),
        "{error}"
    );
    assert_eq!(
        registry.statuses()[0].state,
        McpServerStateLabel::Idle,
        "a denied launch leaves the server unspawned"
    );

    // After approval the same paths connect and work.
    registry.mark_server_launch_approved("fake");
    assert_eq!(registry.tool_schemas(true, &no_cancel()).len(), 1);
    assert_eq!(registry.statuses()[0].state, McpServerStateLabel::Ready);
}

#[test]
fn malformed_config_errors_with_the_file_path() {
    let workspace = temp_workspace();
    fs::create_dir_all(workspace.join(".medusa")).unwrap();
    fs::write(workspace.join(".medusa/mcp.json"), "{not json").unwrap();

    let error = McpRegistry::load(&workspace).unwrap_err();

    assert!(error.to_string().contains("mcp.json"), "{error}");
}

#[test]
fn config_parses_servers_with_args_env_and_read_only() {
    let workspace = temp_workspace();
    fs::create_dir_all(workspace.join(".medusa")).unwrap();
    fs::write(
        workspace.join(".medusa/mcp.json"),
        r#"{"servers": {"docs": {"command": "python3", "args": ["-u", "server.py"],
                "env": {"TOKEN": "x"}, "readOnly": true}}}"#,
    )
    .unwrap();

    let registry = McpRegistry::load(&workspace).unwrap();

    assert!(registry.has_server("docs"));
    assert!(registry.server_marked_read_only("docs"));
    let status = &registry.statuses()[0];
    assert_eq!(status.command_line, "python3 -u server.py");
    assert_eq!(status.state, McpServerStateLabel::Idle);
}

#[test]
fn handshake_discovers_namespaced_tool_schemas() {
    let workspace = write_fake_server_workspace("fake", &[], false);
    let registry = loaded(&workspace);

    let schemas = registry.tool_schemas(true, &no_cancel());

    assert_eq!(schemas.len(), 1);
    assert_eq!(schemas[0]["type"], "function");
    assert_eq!(schemas[0]["name"], "mcp_fake_echo");
    assert!(
        schemas[0]["description"]
            .as_str()
            .unwrap()
            .contains("Echo text back")
    );
    assert_eq!(schemas[0]["parameters"]["type"], "object");
    assert_eq!(
        registry.lookup("mcp_fake_echo"),
        Some(("fake".to_string(), "echo".to_string()))
    );

    let status = &registry.statuses()[0];
    assert_eq!(status.state, McpServerStateLabel::Ready);
    assert_eq!(status.tools, vec!["mcp_fake_echo".to_string()]);
}

#[test]
fn tools_list_follows_next_cursor_pagination() {
    let workspace = write_fake_server_workspace("fake", &[("FAKE_PAGINATE", "1")], false);
    let registry = loaded(&workspace);

    let schemas = registry.tool_schemas(true, &no_cancel());

    let names: Vec<&str> = schemas
        .iter()
        .filter_map(|schema| schema["name"].as_str())
        .collect();
    assert_eq!(names, vec!["mcp_fake_echo", "mcp_fake_extra"]);
}

#[test]
fn unsupported_protocol_version_fails_the_connection() {
    let workspace = write_fake_server_workspace("fake", &[("FAKE_BAD_VERSION", "1")], false);
    let registry = loaded(&workspace);

    assert!(registry.tool_schemas(true, &no_cancel()).is_empty());
    let status = &registry.statuses()[0];
    match &status.state {
        McpServerStateLabel::Failed(error) => {
            assert!(error.contains("1999-01-01"), "{error}");
        }
        other => panic!("expected Failed state, got {other:?}"),
    }
}

#[test]
fn read_only_gating_hides_side_effect_servers() {
    let workspace = write_fake_server_workspace("fake", &[], false);
    let registry = loaded(&workspace);

    // Not marked readOnly: hidden from read-only turns, visible otherwise.
    assert!(registry.tool_schemas(false, &no_cancel()).is_empty());
    assert_eq!(registry.tool_schemas(true, &no_cancel()).len(), 1);

    let workspace = write_fake_server_workspace("safe", &[], true);
    let registry = loaded(&workspace);
    assert_eq!(registry.tool_schemas(false, &no_cancel()).len(), 1);
}

#[test]
fn call_tool_round_trips_text_and_is_error() {
    let workspace = write_fake_server_workspace("fake", &[], false);
    let registry = loaded(&workspace);

    let ok = registry
        .call_tool(
            "fake",
            "echo",
            &json!({"text": "hi"}),
            call_timeout(),
            &no_cancel(),
        )
        .unwrap();
    assert_eq!(ok.text, "echo: hi");
    assert!(!ok.is_error);

    let error = registry
        .call_tool(
            "fake",
            "echo",
            &json!({"error": true}),
            call_timeout(),
            &no_cancel(),
        )
        .unwrap();
    assert!(error.is_error);
    assert_eq!(error.text, "boom");

    let nontext = registry
        .call_tool(
            "fake",
            "echo",
            &json!({"nontext": true}),
            call_timeout(),
            &no_cancel(),
        )
        .unwrap();
    assert!(nontext.text.contains("[non-text content: image omitted]"));
    assert!(nontext.text.contains("with image"));
}

#[test]
fn server_ping_is_answered_with_a_success_result() {
    // Finding 17: a server-initiated `ping` must get an empty result, not
    // -32601 (keepalive servers drop the connection otherwise). The fake
    // server reports "ping_ok" only if the client answered with a result.
    let workspace = write_fake_server_workspace("fake", &[], false);
    let registry = loaded(&workspace);

    let outcome = registry
        .call_tool(
            "fake",
            "echo",
            &json!({"ping_probe": true}),
            call_timeout(),
            &no_cancel(),
        )
        .unwrap();

    assert_eq!(
        outcome.text, "ping_ok",
        "client must answer ping with a result"
    );
    assert!(!outcome.is_error);
}

#[test]
fn call_bails_promptly_when_the_cancel_token_flips_mid_call() {
    // Finding 6: an MCP call parked waiting on a wedged server must abort
    // within a poll interval of the cancel token flipping, not after the
    // full tool timeout.
    let workspace = write_fake_server_workspace("fake", &[], false);
    let registry = loaded(&workspace);

    let cancel = CancelToken::new();
    let flipper = cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        flipper.cancel();
    });

    let started = std::time::Instant::now();
    let error = registry
        .call_tool(
            "fake",
            "echo",
            &json!({"wedge": true}),
            // A 60s timeout would be the pre-fix wait; cancel must win.
            Duration::from_secs(60),
            &cancel,
        )
        .unwrap_err();

    assert!(
        crate::cancel::error_is_cancellation(&error),
        "expected cancellation, got: {error}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "cancel must interrupt the wedged call promptly (took {:?})",
        started.elapsed()
    );
}

#[test]
fn timeout_discards_late_reply_and_connection_stays_usable() {
    let workspace = write_fake_server_workspace("fake", &[], false);
    let registry = loaded(&workspace);

    let started = std::time::Instant::now();
    let error = registry
        .call_tool(
            "fake",
            "echo",
            &json!({"sleep": 1.0, "text": "slow"}),
            Duration::from_millis(150),
            &no_cancel(),
        )
        .unwrap_err();
    assert!(error.to_string().contains("timed out"), "{error}");
    assert!(started.elapsed() < Duration::from_secs(5));

    // Wait out the sleeping reply so it arrives as a late (discarded)
    // response, then verify the next call still correlates correctly.
    std::thread::sleep(Duration::from_millis(1_200));
    let ok = registry
        .call_tool(
            "fake",
            "echo",
            &json!({"text": "after"}),
            call_timeout(),
            &no_cancel(),
        )
        .unwrap();
    assert_eq!(ok.text, "echo: after");
    assert_eq!(
        registry.statuses()[0].restarts,
        0,
        "timeout must not respawn"
    );
}

#[test]
fn garbage_stdout_lines_are_skipped() {
    let workspace = write_fake_server_workspace("fake", &[], false);
    let registry = loaded(&workspace);

    let ok = registry
        .call_tool(
            "fake",
            "echo",
            &json!({"garbage": true, "text": "still works"}),
            call_timeout(),
            &no_cancel(),
        )
        .unwrap();

    assert_eq!(ok.text, "echo: still works");
}

#[test]
fn mid_call_crash_is_not_silently_retried() {
    // Finding 16: a server that performs its side effect then dies before
    // replying must NOT be respawned and re-invoked — that would run a
    // non-idempotent tool twice. The side-effect file must hold exactly
    // one line, and the error must say the call was not retried.
    let workspace = write_fake_server_workspace("fake", &[], false);
    let registry = loaded(&workspace);
    let side_effect = workspace.join("side_effect.log");
    let side_effect_str = side_effect.to_string_lossy().to_string();

    let error = registry
        .call_tool(
            "fake",
            "echo",
            &json!({"count_file": side_effect_str, "crash": true}),
            call_timeout(),
            &no_cancel(),
        )
        .unwrap_err();
    assert!(error.to_string().contains("not retried"), "{error}");

    let lines = fs::read_to_string(&side_effect).unwrap();
    assert_eq!(
        lines.lines().count(),
        1,
        "the crashing call must execute exactly once (no silent replay)"
    );
    assert_eq!(
        registry.statuses()[0].restarts,
        0,
        "a mid-call crash must not respawn the server for this call"
    );

    // A *later* call may reconnect (respawn happens for the next call, not
    // as a replay of the crashed one).
    let ok = registry
        .call_tool(
            "fake",
            "echo",
            &json!({"text": "back"}),
            call_timeout(),
            &no_cancel(),
        )
        .unwrap();
    assert_eq!(ok.text, "echo: back");
    assert_eq!(registry.statuses()[0].restarts, 1);
}

#[test]
fn restart_cap_pins_failed_then_recovers() {
    let workspace = write_fake_server_workspace("fake", &[], false);
    let registry = loaded(&workspace);

    // Each crash kills the server without a respawn+retry; the following
    // normal call reconnects, consuming one restart. Exhaust the budget.
    for _ in 0..MAX_RESTARTS {
        assert!(
            registry
                .call_tool(
                    "fake",
                    "echo",
                    &json!({"crash": true}),
                    call_timeout(),
                    &no_cancel()
                )
                .is_err()
        );
        let ok = registry
            .call_tool(
                "fake",
                "echo",
                &json!({"text": "back"}),
                call_timeout(),
                &no_cancel(),
            )
            .unwrap();
        assert_eq!(ok.text, "echo: back");
    }

    // Budget spent. One more crash, then the reconnect attempt trips the
    // cap and pins Failed with the /mcp restart hint.
    assert!(
        registry
            .call_tool(
                "fake",
                "echo",
                &json!({"crash": true}),
                call_timeout(),
                &no_cancel()
            )
            .is_err()
    );
    let error = registry
        .call_tool(
            "fake",
            "echo",
            &json!({"text": "nope"}),
            call_timeout(),
            &no_cancel(),
        )
        .unwrap_err();
    assert!(error.to_string().contains("/mcp restart"), "{error}");
    assert!(matches!(
        registry.statuses()[0].state,
        McpServerStateLabel::Failed(_)
    ));

    // /mcp restart resets the budget and reconnects.
    registry.restart("fake").unwrap();
    let ok = registry
        .call_tool(
            "fake",
            "echo",
            &json!({"text": "again"}),
            call_timeout(),
            &no_cancel(),
        )
        .unwrap();
    assert_eq!(ok.text, "echo: again");
    assert_eq!(registry.statuses()[0].state, McpServerStateLabel::Ready);
}

#[test]
fn shutdown_reaps_the_child_process() {
    let workspace = write_fake_server_workspace("fake", &[], false);
    let registry = loaded(&workspace);
    registry.tool_schemas(true, &no_cancel());

    let pid = {
        let server = registry.servers.get("fake").unwrap();
        let state = server.state.lock().unwrap();
        match &*state {
            ServerState::Ready { connection, .. } => connection.pid() as i32,
            _ => panic!("server should be ready"),
        }
    };

    registry.shutdown();

    // The fake server exits on stdin EOF; give the OS a moment to reap.
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        if !alive {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "MCP child survived shutdown"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn shutdown_blocks_any_later_spawn() {
    // Finding 19: once shutdown has run, a racing call (e.g. an in-flight
    // worker thread) must not spawn a server that would outlive the
    // process. The launch is approved yet ensure_ready still refuses.
    let workspace = write_fake_server_workspace("fake", &[], false);
    let registry = loaded(&workspace);

    registry.shutdown();

    let error = registry
        .call_tool(
            "fake",
            "echo",
            &json!({"text": "hi"}),
            call_timeout(),
            &no_cancel(),
        )
        .unwrap_err();
    assert!(error.to_string().contains("shutting down"), "{error}");
    assert_eq!(
        registry.statuses()[0].state,
        McpServerStateLabel::Idle,
        "no process may start after shutdown"
    );
}

#[test]
fn namespaced_names_sanitize_cap_and_stay_distinct() {
    assert_eq!(namespaced_tool_name("fake", "echo"), "mcp_fake_echo");
    assert_eq!(
        namespaced_tool_name("my.server", "read file!"),
        "mcp_my_server_read_file_"
    );

    let long_a = namespaced_tool_name("server", &"a".repeat(120));
    let long_b = namespaced_tool_name("server", &format!("{}b", "a".repeat(119)));
    assert_eq!(long_a.len(), MAX_TOOL_NAME_LEN);
    assert_eq!(long_b.len(), MAX_TOOL_NAME_LEN);
    assert_ne!(long_a, long_b, "hash suffix keeps long names distinct");
}

#[test]
fn lookup_round_trips_server_names_containing_underscores() {
    let workspace = write_fake_server_workspace("my_server", &[], false);
    let registry = loaded(&workspace);

    let schemas = registry.tool_schemas(true, &no_cancel());

    assert_eq!(schemas[0]["name"], "mcp_my_server_echo");
    assert_eq!(
        registry.lookup("mcp_my_server_echo"),
        Some(("my_server".to_string(), "echo".to_string()))
    );
}

#[test]
fn colliding_namespaced_names_keep_the_first_registration() {
    let registry = McpRegistry::empty();
    let tool = json!({"name": "run", "description": "", "inputSchema": {"type": "object"}});

    // `a.b` and `a_b` both sanitize to `a_b`: second registration loses.
    let first = registry.register_tools("a.b", std::slice::from_ref(&tool));
    let second = registry.register_tools("a_b", std::slice::from_ref(&tool));

    assert_eq!(first.len(), 1);
    assert!(second.is_empty(), "collision must be skipped");
    assert_eq!(
        registry.lookup("mcp_a_b_run"),
        Some(("a.b".to_string(), "run".to_string()))
    );
}

#[test]
fn malformed_input_schema_is_replaced_with_a_permissive_object() {
    // Finding 15: a tool whose inputSchema is null / non-object / missing
    // must not emit `"parameters": null` (or a non-object) into the model
    // request body — one bad tool would 400 every turn. Each gets a
    // permissive {"type":"object"} substituted so good tools survive.
    let registry = McpRegistry::empty();
    let raw = vec![
        json!({ "name": "null_schema", "description": "", "inputSchema": null }),
        json!({ "name": "string_schema", "description": "", "inputSchema": "nope" }),
        json!({ "name": "array_schema", "description": "", "inputSchema": [1, 2, 3] }),
        json!({ "name": "missing_schema", "description": "" }),
        json!({
            "name": "good_schema", "description": "",
            "inputSchema": { "type": "object", "properties": { "x": { "type": "string" } } }
        }),
    ];

    let tools = registry.register_tools("srv", &raw);

    assert_eq!(tools.len(), 5);
    for tool in &tools {
        assert!(
            tool.parameters.is_object(),
            "{} parameters must be an object, got {:?}",
            tool.name,
            tool.parameters
        );
        assert_eq!(
            tool.parameters["type"], "object",
            "{} must advertise an object schema",
            tool.name
        );
    }
    // The valid schema is preserved verbatim (properties intact).
    let good = tools.iter().find(|t| t.name == "good_schema").unwrap();
    assert_eq!(good.parameters["properties"]["x"]["type"], "string");
}

#[test]
fn sanitize_input_schema_substitutes_only_non_objects() {
    assert_eq!(sanitize_input_schema(None), json!({ "type": "object" }));
    assert_eq!(
        sanitize_input_schema(Some(&Value::Null)),
        json!({ "type": "object" })
    );
    assert_eq!(
        sanitize_input_schema(Some(&json!("string"))),
        json!({ "type": "object" })
    );
    assert_eq!(
        sanitize_input_schema(Some(&json!([1, 2]))),
        json!({ "type": "object" })
    );
    let object = json!({ "type": "object", "required": ["a"] });
    assert_eq!(sanitize_input_schema(Some(&object)), object);
}
