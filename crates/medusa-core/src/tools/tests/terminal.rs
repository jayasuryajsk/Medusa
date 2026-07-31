use super::*;

#[cfg(target_os = "macos")]
use std::ffi::OsStr;

#[test]
fn terminal_exec_runs_command() {
    let runtime = ToolRuntime::new(temp_workspace()).unwrap();

    let result = runtime
        .terminal_exec(TerminalExecRequest::new("printf medusa"))
        .unwrap();

    assert_eq!(result.code, Some(0));
    assert_eq!(result.stdout, "medusa");
}

#[test]
fn ask_mode_without_handler_auto_denies_mutations() {
    let workspace = ask_workspace();
    let runtime = ToolRuntime::new(&workspace).unwrap();

    let error = runtime
        .terminal_exec(TerminalExecRequest::new("touch created.txt"))
        .unwrap_err();
    assert!(error.to_string().contains("auto-denied"));
    assert!(!workspace.join("created.txt").exists());

    let error = runtime
        .file_edit(FileEditRequest::new("new.txt", "", "hello"))
        .unwrap_err();
    assert!(error.to_string().contains("auto-denied"));
    assert!(!workspace.join("new.txt").exists());

    // Safe reads never hit the gate.
    runtime
        .terminal_exec(TerminalExecRequest::new("ls"))
        .unwrap();
}

#[test]
fn approval_handler_decisions_control_execution() {
    let workspace = ask_workspace();
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen_in_handler = Arc::clone(&seen);
    let runtime = ToolRuntime::new(&workspace)
        .unwrap()
        .with_approval_handler(Arc::new(move |request: ApprovalRequest| {
            let deny = request
                .command
                .as_deref()
                .is_some_and(|command| command.contains("deny-me"));
            seen_in_handler.lock().unwrap().push(request);
            if deny {
                ApprovalDecision::Deny
            } else {
                ApprovalDecision::AllowOnce
            }
        }));

    runtime
        .terminal_exec(TerminalExecRequest::new("mkdir approved-dir"))
        .unwrap();
    assert!(workspace.join("approved-dir").exists());

    let error = runtime
        .terminal_exec(TerminalExecRequest::new("touch deny-me.txt"))
        .unwrap_err();
    assert!(error.to_string().contains("denied by user"));
    assert!(!workspace.join("deny-me.txt").exists());

    runtime
        .file_edit(FileEditRequest::new("approved.txt", "", "content"))
        .unwrap();
    assert!(workspace.join("approved.txt").exists());

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 3);
    assert_eq!(seen[0].tool, ApprovalTool::TerminalExec);
    assert_eq!(seen[2].tool, ApprovalTool::FileEdit);
    assert_eq!(seen[2].paths, vec!["approved.txt".to_string()]);
}

#[test]
fn pre_cancelled_token_stops_foreground_terminal_exec_before_spawn() {
    let workspace = temp_workspace();
    let cancel = crate::cancel::CancelToken::new();
    cancel.cancel();
    let runtime = ToolRuntime::new(&workspace)
        .unwrap()
        .with_cancel_token(cancel);

    let error = runtime
        .terminal_exec(TerminalExecRequest::new("touch never-created.txt"))
        .unwrap_err();

    assert!(crate::cancel::error_is_cancellation(&error), "{error}");
    assert!(!workspace.join("never-created.txt").exists());
}

#[test]
fn cancelling_mid_run_kills_a_foreground_command_promptly() {
    let workspace = temp_workspace();
    let cancel = crate::cancel::CancelToken::new();
    let canceller = cancel.clone();
    let runtime = ToolRuntime::new(&workspace)
        .unwrap()
        .with_cancel_token(cancel);
    thread::spawn(move || {
        thread::sleep(std::time::Duration::from_millis(120));
        canceller.cancel();
    });

    let started = Instant::now();
    let error = runtime
        .terminal_exec(TerminalExecRequest::new("sleep 30"))
        .unwrap_err();

    assert!(error.to_string().contains("cancelled"), "{error}");
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
}

#[test]
fn cancelled_token_denies_approvals_without_invoking_the_handler() {
    let workspace = ask_workspace();
    let cancel = crate::cancel::CancelToken::new();
    cancel.cancel();
    let runtime = ToolRuntime::new(&workspace)
        .unwrap()
        .with_cancel_token(cancel)
        .with_approval_handler(Arc::new(|request: ApprovalRequest| {
            panic!("cancelled turn must not prompt: {request:?}")
        }));

    let error = runtime
        .terminal_exec(TerminalExecRequest::new("touch never-created.txt"))
        .unwrap_err();

    assert!(crate::cancel::error_is_cancellation(&error), "{error}");
    assert!(!workspace.join("never-created.txt").exists());
}

#[test]
fn background_jobs_ignore_the_cancel_token() {
    let workspace = temp_workspace();
    let cancel = crate::cancel::CancelToken::new();
    cancel.cancel();
    let runtime = ToolRuntime::new(&workspace)
        .unwrap()
        .with_cancel_token(cancel);

    // Background jobs outlive the turn by design; the token must not
    // gate or kill them.
    let result = runtime
        .terminal_exec(TerminalExecRequest::background("printf bg"))
        .unwrap();

    assert!(result.background);
    assert!(result.pid.is_some());
}

fn guarded_workspace() -> PathBuf {
    let workspace = temp_workspace();
    crate::permissions::PermissionPolicy::write_mode(
        &workspace,
        crate::permissions::PermissionMode::Guarded,
    )
    .unwrap();
    workspace
}

fn escalation_request(command: &str) -> TerminalExecRequest {
    let mut request = TerminalExecRequest::new(command);
    request.unsandboxed = true;
    request
}

#[test]
fn sandbox_escalation_without_a_handler_is_auto_denied() {
    let runtime = ToolRuntime::new(guarded_workspace()).unwrap();

    // `printf hi` would auto-run in guarded mode, but escaping the
    // sandbox always takes a fresh human decision.
    let error = runtime
        .terminal_exec(escalation_request("printf hi"))
        .unwrap_err();

    assert!(error.to_string().contains("requires approval"), "{error}");
}

#[test]
fn approved_sandbox_escalation_runs_unsandboxed() {
    let workspace = guarded_workspace();
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen_in_handler = Arc::clone(&seen);
    let runtime = ToolRuntime::new(&workspace)
        .unwrap()
        .with_sandbox(crate::sandbox::SandboxPolicy::new(true, false, Vec::new()))
        .with_approval_handler(Arc::new(move |request: ApprovalRequest| {
            seen_in_handler.lock().unwrap().push(request);
            ApprovalDecision::AllowOnce
        }));

    let result = runtime
        .terminal_exec(escalation_request("printf escaped"))
        .unwrap();

    assert_eq!(result.code, Some(0));
    assert_eq!(result.stdout, "escaped");
    assert!(!result.sandboxed, "approved escalation must skip the wrap");
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert!(seen[0].sandbox_escalation);
    assert_eq!(seen[0].command.as_deref(), Some("printf escaped"));
}

#[test]
fn denied_sandbox_escalation_does_not_run() {
    let workspace = guarded_workspace();
    let runtime = ToolRuntime::new(&workspace)
        .unwrap()
        .with_approval_handler(Arc::new(|_request| ApprovalDecision::Deny));

    let error = runtime
        .terminal_exec(escalation_request("touch escaped.txt"))
        .unwrap_err();

    assert!(error.to_string().contains("denied by user"), "{error}");
    assert!(!workspace.join("escaped.txt").exists());
}

#[test]
fn readonly_mode_refuses_sandbox_escalation_without_prompting() {
    let workspace = temp_workspace();
    crate::permissions::PermissionPolicy::write_mode(
        &workspace,
        crate::permissions::PermissionMode::Readonly,
    )
    .unwrap();
    let runtime = ToolRuntime::new(&workspace)
        .unwrap()
        .with_approval_handler(Arc::new(|request: ApprovalRequest| {
            panic!("readonly escalation must not prompt: {request:?}")
        }));

    // `pwd` is allowed in readonly mode, but never unsandboxed.
    let error = runtime
        .terminal_exec(escalation_request("pwd"))
        .unwrap_err();

    assert!(error.to_string().contains("readonly"), "{error}");
}

#[cfg(target_os = "macos")]
#[test]
fn live_sandboxed_terminal_exec_confines_writes_and_marks_results() {
    use crate::sandbox::{SandboxAvailability, SandboxPolicy, sandbox_availability};
    if *sandbox_availability() != SandboxAvailability::Available {
        eprintln!("skipping: sandbox-exec unavailable on this machine");
        return;
    }

    let workspace = temp_workspace();
    let runtime = ToolRuntime::new(&workspace)
        .unwrap()
        .with_sandbox(SandboxPolicy::new(true, false, Vec::new()));

    // Plain commands and workspace writes succeed and are marked.
    let ok = runtime
        .terminal_exec(TerminalExecRequest::new("echo hi && touch inside.txt"))
        .unwrap();
    assert_eq!(ok.code, Some(0), "stderr: {}", ok.stderr);
    assert!(ok.sandboxed);
    assert_eq!(ok.stdout.trim(), "hi");
    assert!(workspace.join("inside.txt").exists());

    // Children see the sandbox advertised in their environment.
    let env = runtime
        .terminal_exec(TerminalExecRequest::new("printenv MEDUSA_SANDBOX"))
        .unwrap();
    assert_eq!(env.stdout.trim(), "seatbelt");

    // Writes outside every writable root are denied (HOME is never a
    // default root); clean up if the sandbox ever failed open.
    let home = PathBuf::from(std::env::var_os("HOME").expect("HOME set"));
    let outside = home.join(format!(
        "medusa-tools-sandbox-escape-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let denied = runtime
        .terminal_exec(TerminalExecRequest::new(format!(
            "touch '{}'",
            outside.display()
        )))
        .unwrap();
    let escaped = outside.exists();
    let _ = fs::remove_file(&outside);
    assert!(!escaped, "sandboxed command wrote outside its roots");
    assert!(denied.sandboxed);
    assert_ne!(denied.code, Some(0));
    assert!(
        crate::sandbox::looks_sandbox_denied(&denied.stderr, denied.code),
        "stderr should look like a sandbox denial: {}",
        denied.stderr
    );
}

#[cfg(target_os = "macos")]
#[test]
fn strict_probes_sandbox_with_network_denied_even_when_policy_is_lax() {
    use crate::sandbox::{SandboxAvailability, SandboxPolicy, sandbox_availability};
    if *sandbox_availability() != SandboxAvailability::Available {
        eprintln!("skipping: sandbox-exec unavailable on this machine");
        return;
    }

    let workspace = temp_workspace();
    // Open-style stance: sandbox disabled, network allowed when it is on.
    let runtime = ToolRuntime::new(&workspace)
        .unwrap()
        .with_sandbox(SandboxPolicy::new(false, true, Vec::new()));

    // Strict (explore-probe) commands still sandbox, with network denied.
    let (command, sandboxed) = runtime
        .build_shell_command("cat README.md", runtime.workspace(), true, false)
        .unwrap();
    assert!(sandboxed);
    assert_eq!(command.get_program(), OsStr::new("/usr/bin/sandbox-exec"));
    assert!(command.get_envs().any(|(key, value)| {
        key == OsStr::new("MEDUSA_SANDBOX_NETWORK_DISABLED") && value == Some(OsStr::new("1"))
    }));

    // Ordinary commands under a disabled policy stay plain.
    let (_, sandboxed) = runtime
        .build_shell_command("echo hi", runtime.workspace(), false, false)
        .unwrap();
    assert!(!sandboxed);
}

#[test]
fn explore_probes_never_prompt_in_ask_mode() {
    let workspace = ask_workspace();
    fs::write(workspace.join("README.md"), "medusa\n").unwrap();
    let runtime = ToolRuntime::new(&workspace)
        .unwrap()
        .with_approval_handler(Arc::new(|request: ApprovalRequest| {
            panic!("probe should not prompt: {request:?}")
        }));

    let result = runtime
        .explore_batch(ExploreBatchRequest {
            goal: "probe".to_string(),
            probes: vec![ExploreProbe {
                kind: ExploreProbeKind::Terminal,
                query: None,
                path: None,
                paths: Vec::new(),
                command: Some("cat README.md".to_string()),
                cwd: None,
                start_line: None,
                end_line: None,
                depth: None,
                max_results: None,
                max_entries: None,
                case_sensitive: None,
            }],
        })
        .unwrap();

    match crate::sandbox::sandbox_availability() {
        SandboxAvailability::Available => assert_eq!(result.failed, 0),
        SandboxAvailability::Broken(_) | SandboxAvailability::UnsupportedPlatform => {
            assert_eq!(result.failed, 1);
            assert!(
                result.probes[0]
                    .output
                    .contains("sandbox-required command blocked"),
                "{}",
                result.probes[0].output
            );
        }
    }
}
