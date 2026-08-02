use super::*;

fn queue_approval(app: &mut App, command: &str) -> mpsc::Receiver<ApprovalDecision> {
    queue_approval_kind(app, command, false)
}

fn queue_approval_kind(
    app: &mut App,
    command: &str,
    sandbox_escalation: bool,
) -> mpsc::Receiver<ApprovalDecision> {
    let (respond, decision) = mpsc::channel();
    app.approval_queue.push_back(PendingApproval {
        request: ApprovalRequest {
            tool: ApprovalTool::TerminalExec,
            command: Some(command.to_string()),
            paths: Vec::new(),
            background: false,
            sandbox_escalation,
        },
        respond,
    });
    // Pretend the prompt has been visible past the grace window so the
    // decision keys act immediately in tests.
    app.approval_shown_at = Instant::now().checked_sub(APPROVAL_KEY_GRACE * 2);
    decision
}

#[test]
fn approval_keys_resolve_and_unblock_worker() {
    let mut app = app();
    let decision = queue_approval(&mut app, "cargo build");

    app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));

    assert_eq!(decision.try_recv().unwrap(), ApprovalDecision::AllowOnce);
    assert!(app.approval_queue.is_empty());
    assert_eq!(app.status_line, "approved once");
}

#[test]
fn approval_deny_remembers_command_for_turn() {
    let mut app = app();
    let first = queue_approval(&mut app, "touch scary.txt");
    app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
    assert_eq!(first.try_recv().unwrap(), ApprovalDecision::Deny);

    // A verbatim retry auto-denies without prompting.
    assert_eq!(
        app.auto_approval_decision(&ApprovalRequest {
            tool: ApprovalTool::TerminalExec,
            command: Some("touch scary.txt".to_string()),
            paths: Vec::new(),
            background: false,
            sandbox_escalation: false,
        }),
        Some(ApprovalDecision::Deny)
    );
}

#[test]
fn escape_denies_pending_approval_without_quitting() {
    let mut app = app();
    let decision = queue_approval(&mut app, "cargo build");

    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

    assert_eq!(decision.try_recv().unwrap(), ApprovalDecision::Deny);
    assert!(!app.should_quit);

    // The armed-quit state was reset: next Esc only arms.
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(!app.should_quit);
}

fn running_tool_row(name: &str) -> TranscriptItem {
    TranscriptItem::Tool(ToolRun {
        id: Some("call_running".to_string()),
        started_at: Instant::now(),
        pending_result: None,
        name: name.to_string(),
        summary: format!("$ {name}"),
        state: ToolRunState::Running,
        detail: String::new(),
        expanded: false,
        group_expanded: false,
    })
}

/// App in the "worker streaming" state with an attached cancel token.
fn working_app() -> (App, mpsc::Sender<ModelStreamEvent>, CancelToken) {
    let mut app = app();
    let (sender, receiver) = mpsc::channel::<ModelStreamEvent>();
    app.model_events = Some(receiver);
    let token = CancelToken::new();
    app.turn_cancel = Some(token.clone());
    app.turn_started_at = Some(Instant::now());
    (app, sender, token)
}

#[test]
fn escape_while_working_requests_cancel_instead_of_arming_quit() {
    let (mut app, _sender, token) = working_app();

    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

    assert!(!app.should_quit);
    assert!(token.is_cancelled());
    assert!(app.cancel_requested_at.is_some());
    assert!(
        app.status_line.contains("cancelling"),
        "{}",
        app.status_line
    );
    // Cancelling must not arm double-esc quit, and the receiver stays
    // attached so the worker can still report Cancelled.
    assert!(app.last_escape_at.is_none());
    assert!(app.model_events.is_some());
}

#[test]
fn second_escape_force_abandons_the_turn() {
    let (mut app, _sender, _token) = working_app();
    app.transcript.push(running_tool_row("terminal.exec"));

    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

    assert!(!app.should_quit);
    assert!(app.model_events.is_none());
    assert!(app.turn_cancel.is_none());
    assert!(app.cancel_requested_at.is_none());
    assert_eq!(app.status_line, "turn abandoned");
    assert!(matches!(
        &app.transcript[0],
        TranscriptItem::Tool(run)
            if run.state == ToolRunState::Failed && run.detail == "cancelled"
    ));

    // Idle again: the classic arm-then-quit gesture still works.
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(!app.should_quit);
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(app.should_quit);
}

#[test]
fn escape_clears_input_before_cancelling_a_working_turn() {
    let (mut app, _sender, token) = working_app();
    app.input = "half-typed follow-up".to_string();
    app.input_cursor = app.input_len();

    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(app.input.is_empty());
    assert!(!token.is_cancelled());

    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(token.is_cancelled());
    assert!(!app.should_quit);
}

#[test]
fn request_cancel_denies_every_queued_approval() {
    let (mut app, _sender, token) = working_app();
    let first = queue_approval(&mut app, "cargo build");
    let second = queue_approval(&mut app, "cargo test");

    app.request_cancel_turn();

    assert!(token.is_cancelled());
    assert!(app.approval_queue.is_empty());
    assert_eq!(first.try_recv().unwrap(), ApprovalDecision::Deny);
    assert_eq!(second.try_recv().unwrap(), ApprovalDecision::Deny);
}

#[test]
fn approvals_arriving_after_cancel_are_denied_and_unblock_the_worker() {
    let mut app = app();
    app.cancel_requested_at = Some(Instant::now());

    // A tool worker parked in the approval handler, its request racing
    // the cancel: the drain must answer it with Deny, never queue it.
    let handler = app.approval_handler.clone();
    let worker = thread::spawn(move || {
        handler(ApprovalRequest {
            tool: ApprovalTool::TerminalExec,
            command: Some("cargo build".to_string()),
            paths: Vec::new(),
            background: false,
            sandbox_escalation: false,
        })
    });

    let deadline = Instant::now() + Duration::from_secs(2);
    while !app.drain_approval_requests() {
        assert!(Instant::now() < deadline, "approval request never arrived");
        thread::sleep(Duration::from_millis(10));
    }

    assert!(app.approval_queue.is_empty());
    assert_eq!(worker.join().unwrap(), ApprovalDecision::Deny);
}

#[test]
fn cancelled_event_finalizes_the_turn_and_preserves_partial_text() {
    let (mut app, sender, _token) = working_app();
    app.cancel_requested_at = Some(Instant::now());
    app.transcript
        .push(TranscriptItem::Message(ChatMessage::user("do the thing")));
    app.transcript
        .push(TranscriptItem::Message(ChatMessage::assistant(
            "partial answer",
        )));
    app.streaming_message = Some(1);
    app.transcript.push(running_tool_row("terminal.exec"));
    app.queued_turns.push_back("queued task".to_string());

    sender.send(ModelStreamEvent::Cancelled).unwrap();
    app.drain_model_events();

    assert!(app.model_events.is_none());
    assert!(app.turn_cancel.is_none());
    assert!(app.cancel_requested_at.is_none());
    assert!(app.streaming_message.is_none());
    // [21]: the acknowledged queued prompt is kept, not silently dropped.
    assert_eq!(
        app.queued_turns
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec!["queued task"],
    );
    assert_eq!(app.status_line, "turn interrupted");
    assert!(matches!(
        &app.transcript[2],
        TranscriptItem::Tool(run)
            if run.state == ToolRunState::Failed && run.detail == "cancelled"
    ));
    assert!(matches!(
        app.transcript.last(),
        Some(TranscriptItem::Message(message))
            if message.role == ChatRole::System && message.content == TURN_INTERRUPTED_NOTE
    ));

    // Resumability: history keeps the partial assistant text, gains the
    // interruption note, and the app is idle so a fresh turn (with a
    // fresh token) can start.
    let history = app.conversation_history();
    assert!(
        history
            .iter()
            .any(|message| message.role == "assistant" && message.content == "partial answer")
    );
    assert!(
        history
            .iter()
            .any(|message| message.role == "system" && message.content == TURN_INTERRUPTED_NOTE)
    );
    assert!(!app.is_working());
}

#[test]
fn error_event_while_cancelling_renders_as_interruption_not_failure() {
    let (mut app, sender, _token) = working_app();
    app.cancel_requested_at = Some(Instant::now());

    sender
        .send(ModelStreamEvent::Error(
            "failed to send stream event: receiving on a closed channel".to_string(),
        ))
        .unwrap();
    app.drain_model_events();

    assert!(
        app.toast.is_none(),
        "user-initiated stop must not toast an error"
    );
    assert_eq!(app.status_line, "turn interrupted");
    assert!(app.cancel_requested_at.is_none());
    assert!(matches!(
        app.transcript.last(),
        Some(TranscriptItem::Message(message))
            if message.content == TURN_INTERRUPTED_NOTE
    ));
}

/// [8]: Esc raced a natural finish — the worker's Done is already in the
/// channel when the user asks to stop. The stop intent must win: the turn
/// finalizes as an interruption, never as a clean completion.
#[test]
fn done_racing_esc_stops_the_turn_instead_of_finalizing_normally() {
    let (mut app, sender, token) = working_app();
    token.cancel();
    app.cancel_requested_at = Some(Instant::now());

    sender
        .send(ModelStreamEvent::Done { event_count: 3 })
        .unwrap();
    app.drain_model_events();

    assert!(app.turn_cancel.is_none());
    assert!(app.cancel_requested_at.is_none());
    assert!(!app.is_working());
    assert_eq!(app.status_line, "turn interrupted");
    assert!(
        app.transcript.iter().any(|item| matches!(
            item,
            TranscriptItem::Message(message) if message.content == TURN_INTERRUPTED_NOTE
        )),
        "a stopped turn must leave the interruption note"
    );
}

/// [8] + [21]: Esc racing the natural Done through the real key path must
/// stop the turn and NEVER launch a queued follow-up; the queued prompts
/// are kept for the user.
#[test]
fn esc_racing_done_stops_and_keeps_queued_turns() {
    let (mut app, sender, _token) = working_app();
    app.queued_turns.push_back("queued task one".to_string());
    app.queued_turns.push_back("queued task two".to_string());

    // Worker already finished: Done is sitting in the channel, but the
    // UI has not drained it yet, so is_working() is still true.
    sender
        .send(ModelStreamEvent::Done { event_count: 3 })
        .unwrap();
    assert!(app.is_working());

    // User presses Esc to stop everything (real key path).
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(app.status_line, "cancelling… esc again to force-stop");
    assert!(app.cancel_requested_at.is_some());

    // Next frame drains the channel and hits the Done event.
    app.drain_model_events();

    // [8]: the cancel intent wins — the turn stops.
    assert!(app.cancel_requested_at.is_none());
    assert_eq!(app.status_line, "turn interrupted");
    // [8]: no queued turn was launched — its user message never entered
    // the transcript.
    assert!(
        !app.transcript.iter().any(|item| matches!(
            item,
            TranscriptItem::Message(message)
                if message.role == ChatRole::User && message.content == "queued task one"
        )),
        "a cancel intent must never silently launch the next queued turn"
    );
    // [21]: both queued prompts are kept, in order, not discarded.
    assert_eq!(
        app.queued_turns
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec!["queued task one", "queued task two"],
    );
}

/// [5]: Esc while a background workflow runs (no model turn streaming) must
/// cancel the workflow, never fall through to the double-esc quit arm that
/// would kill subagents mid file_edit/file_patch.
#[test]
fn escape_cancels_background_workflow_instead_of_quitting() {
    let mut app = app();
    let (_sender, receiver) = mpsc::channel::<WorkflowEvent>();
    let workflow = background_workflow(&app, receiver);
    let token = workflow.cancel.clone();
    app.workflow_events.push(workflow);
    let view = WorkflowRunView {
        id: "workflow-test".to_string(),
        title: "refactor auth".to_string(),
        task: "refactor auth".to_string(),
        status: WorkflowViewState::Running,
        phases: Vec::new(),
        summary: String::new(),
        expanded: false,
    };
    app.workflows.push(view.clone());
    app.transcript.push(TranscriptItem::Workflow(view));

    // A background workflow makes is_working() false but keeps work active.
    assert!(!app.is_working());
    assert!(app.has_active_workflows());

    // First Esc: cancel the workflow, mark its row, never arm quit.
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(!app.should_quit);
    assert!(
        token.is_cancelled(),
        "workflow cancel token must be flipped"
    );
    assert!(
        app.last_escape_at.is_none(),
        "cancelling a workflow must not arm the double-esc quit"
    );
    assert_eq!(app.workflows[0].status, WorkflowViewState::Failed);

    // Second Esc while the worker is still attached must still not quit.
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(
        !app.should_quit,
        "double-esc must never quit while a workflow is active"
    );
}

/// [21]: cancelling a turn must keep the queued follow-up prompts (the UI
/// acknowledged each) and tell the user — never silently discard them.
#[test]
fn cancelling_a_turn_keeps_queued_prompts_and_tells_the_user() {
    let (mut app, _sender, _token) = working_app();
    app.queued_turns
        .push_back("also update the changelog".to_string());

    app.finalize_cancelled_turn("turn interrupted");

    assert_eq!(
        app.queued_turns
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec!["also update the changelog"],
    );
    assert!(
        app.toast
            .as_ref()
            .is_some_and(|toast| toast.message.contains("kept")),
        "user must be told the queued prompt was kept: {:?}",
        app.toast.as_ref().map(|toast| &toast.message)
    );
}

/// [21]: kept queued prompts run on an explicit empty submit while idle,
/// never a silent auto-launch.
#[test]
fn empty_submit_runs_kept_queued_prompt_when_idle() {
    let mut app = app();
    app.queued_turns
        .push_back("run the kept prompt".to_string());

    app.submit_input(); // empty composer

    assert!(
        app.queued_turns.is_empty(),
        "empty submit must run the kept prompt"
    );
    assert!(
        app.transcript.iter().any(|item| matches!(
            item,
            TranscriptItem::Message(ChatMessage { role: ChatRole::User, content, .. })
                if content == "run the kept prompt"
        )),
        "the kept prompt must be started"
    );
}

#[test]
fn usage_events_accumulate_across_requests_within_a_turn() {
    let (mut app, sender, _token) = working_app();

    // A tool-looping turn makes one request per iteration; each reports
    // its own usage and the TUI must count all of them.
    sender
        .send(ModelStreamEvent::Usage(TokenUsage {
            input: 100,
            output: 10,
            cached: 40,
        }))
        .unwrap();
    sender
        .send(ModelStreamEvent::Usage(TokenUsage {
            input: 250,
            output: 30,
            cached: 200,
        }))
        .unwrap();
    sender
        .send(ModelStreamEvent::Done { event_count: 5 })
        .unwrap();
    app.drain_model_events();

    let expected = TokenUsage {
        input: 350,
        output: 40,
        cached: 240,
    };
    assert_eq!(app.session_usage, expected);
    assert_eq!(app.session_requests, 2);
    assert_eq!(app.last_turn_usage, expected);
    assert_eq!(app.last_turn_requests, 2);
}

#[test]
fn cancelled_turn_still_freezes_last_turn_usage() {
    let (mut app, sender, _token) = working_app();

    sender
        .send(ModelStreamEvent::Usage(TokenUsage {
            input: 10,
            output: 2,
            cached: 0,
        }))
        .unwrap();
    sender.send(ModelStreamEvent::Cancelled).unwrap();
    app.drain_model_events();

    assert_eq!(app.last_turn_requests, 1);
    assert_eq!(app.last_turn_usage.input, 10);
    assert_eq!(app.session_requests, 1);
}

#[test]
fn token_counts_format_compactly() {
    assert_eq!(format_token_count(0), "0 tok");
    assert_eq!(format_token_count(812), "812 tok");
    assert_eq!(format_token_count(1_234), "1.23k tok");
    assert_eq!(format_token_count(60_000), "60.00k tok");
    assert_eq!(format_token_count(2_050_000), "2.05M tok");
}

#[test]
fn context_report_categories_sum_to_total_estimate() {
    let mut app = app();
    app.transcript = vec![
        TranscriptItem::Message(ChatMessage::user("write the missing tests")),
        TranscriptItem::Reasoning(ReasoningTrace {
            content: "inspecting the test module first".to_string(),
            expanded: false,
        }),
        TranscriptItem::Plan(PlanView {
            summary: "test plan".to_string(),
            items: vec![PlanItemView {
                text: "add coverage".to_string(),
                status: PlanItemStatus::Pending,
                evidence: Vec::new(),
            }],
            expanded: false,
        }),
    ];
    app.push_tool_start("file_read".to_string(), "src/main.rs".to_string());

    let report = app.build_context_report();
    assert!(report.message_tokens > 0);
    // Presentation-only rows are not replayed verbatim to the backend. Their
    // durable state is represented in the generated system-state message.
    assert_eq!(report.tool_tokens, 0);
    assert_eq!(report.reasoning_tokens, 0);
    assert_eq!(report.plan_tokens, 0);
    // The report's total is exactly the sum of its categories.
    assert_eq!(
        report.total_tokens(),
        report.instructions_tokens
            + report.system_tokens
            + report.message_tokens
            + report.tool_tokens
            + report.reasoning_tokens
            + report.plan_tokens
            + report.summary_tokens
    );
    assert!(
        report.instructions_tokens > 0,
        "system prompt estimate missing"
    );
    assert!(report.system_tokens > 0, "session header estimate missing");
    assert!(report.budget >= 1_000);
    assert!(report.summary_covers.is_none());
    assert_eq!(report.summary_tokens, 0);
}

#[test]
fn cost_and_context_commands_open_modals() {
    let mut app = app();

    assert!(app.run_local_tool_command("/cost"));
    assert_eq!(app.active_modal, Some(Modal::Cost));

    app.active_modal = None;
    assert!(app.run_local_tool_command("/context"));
    assert_eq!(app.active_modal, Some(Modal::Context));
    assert!(app.context_report.is_some());
}

#[test]
fn compact_refuses_while_a_turn_is_streaming() {
    let (mut app, _sender, _token) = working_app();

    assert!(app.run_local_tool_command("/compact"));

    assert!(app.compact_events.is_none(), "no compact worker may start");
    assert!(matches!(
        app.toast,
        Some(Toast {
            kind: ToastKind::Warning,
            ..
        })
    ));
}

#[test]
fn compact_result_lands_as_success_toast_with_before_and_after() {
    let mut app = app();
    let (sender, receiver) = mpsc::channel();
    app.compact_events = Some(receiver);
    sender
        .send(Ok(ManualCompaction {
            before_tokens: 12_000,
            after_tokens: 3_000,
            folded_messages: 14,
        }))
        .unwrap();

    assert!(app.drain_compact_events());

    assert!(app.compact_events.is_none());
    assert!(!app.compaction_active);
    assert!(app.last_compaction.is_some());
    let toast = app.toast.clone().expect("compact toast");
    assert_eq!(toast.kind, ToastKind::Success);
    assert!(toast.message.contains("12.00k tok"), "{}", toast.message);
    assert!(toast.message.contains("3.00k tok"), "{}", toast.message);
    assert!(
        toast.message.contains("14 messages folded"),
        "{}",
        toast.message
    );
}

#[test]
fn compact_failure_lands_as_error_toast() {
    let mut app = app();
    let (sender, receiver) = mpsc::channel();
    app.compact_events = Some(receiver);
    sender.send(Err("backend offline".to_string())).unwrap();

    assert!(app.drain_compact_events());

    assert!(app.compact_events.is_none());
    let toast = app.toast.clone().expect("compact toast");
    assert_eq!(toast.kind, ToastKind::Error);
    assert!(
        toast.message.contains("backend offline"),
        "{}",
        toast.message
    );
}

#[test]
fn automatic_compaction_events_explain_the_context_drop() {
    let (mut app, sender, _token) = working_app();
    sender
        .send(ModelStreamEvent::CompactionStarted {
            before_tokens: 90_000,
        })
        .unwrap();
    sender
        .send(ModelStreamEvent::CompactionFinished {
            before_tokens: 90_000,
            after_tokens: 42_000,
            folded_messages: 18,
        })
        .unwrap();

    assert!(app.drain_model_events());

    assert!(!app.compaction_active);
    let (result, _) = app.last_compaction.expect("compaction result");
    assert_eq!(result.before_tokens, 90_000);
    assert_eq!(result.after_tokens, 42_000);
    assert!(app.status_line.contains("context compacted"));
    assert!(
        app.toast
            .as_ref()
            .unwrap()
            .message
            .contains("18 messages folded")
    );
}

#[test]
fn always_allow_settles_queued_siblings_and_persists() {
    let (mut app, workspace) = app_in_workspace();
    let first = queue_approval(&mut app, "cargo test -p medusa-core");
    let sibling = queue_approval(&mut app, "cargo test -p medusa-tui");

    app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));

    assert_eq!(first.try_recv().unwrap(), ApprovalDecision::AlwaysAllow);
    // The sibling with the same derived prefix resolved without a prompt.
    assert_eq!(sibling.try_recv().unwrap(), ApprovalDecision::AllowOnce);
    assert!(app.approval_queue.is_empty());

    let persisted = fs::read_to_string(workspace.join(".medusa/permissions.json")).unwrap();
    assert!(persisted.contains("cargo test"));
}

#[test]
fn approval_keys_do_not_leak_into_composer() {
    let mut app = app();
    let _decision = queue_approval(&mut app, "cargo build");

    app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
    assert!(app.input.is_empty(), "keys must not reach the composer");
    assert_eq!(app.approval_queue.len(), 1);

    app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
    assert!(app.input.is_empty());
    assert!(app.approval_queue.is_empty());
}

#[test]
fn approval_keys_are_ignored_during_grace_window() {
    let mut app = app();
    let (respond, decision) = mpsc::channel();
    app.approval_queue.push_back(PendingApproval {
        request: ApprovalRequest {
            tool: ApprovalTool::TerminalExec,
            command: Some("cargo build".to_string()),
            paths: Vec::new(),
            background: false,
            sandbox_escalation: false,
        },
        respond,
    });
    app.approval_shown_at = Some(Instant::now());

    app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
    assert!(decision.try_recv().is_err());
    assert_eq!(app.approval_queue.len(), 1);

    app.approval_shown_at = Instant::now().checked_sub(APPROVAL_KEY_GRACE * 2);
    app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
    assert_eq!(decision.try_recv().unwrap(), ApprovalDecision::AllowOnce);
}

#[test]
fn ctrl_modified_keys_never_decide_an_approval() {
    let mut app = app();
    let decision = queue_approval(&mut app, "cargo build");

    // Ctrl+A (readline home) must not trigger AlwaysAllow.
    app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
    assert!(decision.try_recv().is_err());
    assert_eq!(app.approval_queue.len(), 1);

    // Plain 'a' still works.
    app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
    assert_eq!(decision.try_recv().unwrap(), ApprovalDecision::AlwaysAllow);
}

#[test]
fn sandbox_escalations_never_settle_against_stored_grants() {
    let mut app = app();
    app.session_terminal_grants.push("cargo build".to_string());

    // The very same command settles from the grant when sandboxed...
    assert_eq!(
        app.auto_approval_decision(&ApprovalRequest {
            tool: ApprovalTool::TerminalExec,
            command: Some("cargo build".to_string()),
            paths: Vec::new(),
            background: false,
            sandbox_escalation: false,
        }),
        Some(ApprovalDecision::AllowOnce)
    );
    // ...but an escalation of it must always reach a human.
    assert_eq!(
        app.auto_approval_decision(&ApprovalRequest {
            tool: ApprovalTool::TerminalExec,
            command: Some("cargo build".to_string()),
            paths: Vec::new(),
            background: false,
            sandbox_escalation: true,
        }),
        None
    );
}

#[test]
fn always_allow_key_cannot_decide_a_sandbox_escalation() {
    let mut app = app();
    let decision = queue_approval_kind(&mut app, "cargo build", true);

    // 'a' is not offered on escalation cards and must be inert.
    app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
    assert!(decision.try_recv().is_err());
    assert_eq!(app.approval_queue.len(), 1);
    assert!(app.session_terminal_grants.is_empty());

    // Allow-once still resolves it without recording any grant.
    app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
    assert_eq!(decision.try_recv().unwrap(), ApprovalDecision::AllowOnce);
    assert!(app.session_terminal_grants.is_empty());
}

#[test]
fn escalation_always_allow_decision_downgrades_to_allow_once() {
    let mut app = app();
    let decision = queue_approval_kind(&mut app, "cargo build", true);

    // Even if an AlwaysAllow decision reaches an escalation through some
    // other path, nothing may be persisted.
    app.resolve_pending_approval(ApprovalDecision::AlwaysAllow);

    assert_eq!(decision.try_recv().unwrap(), ApprovalDecision::AllowOnce);
    assert!(app.session_terminal_grants.is_empty());
}

#[test]
fn env_prefixed_commands_settle_against_grants() {
    let mut app = app();
    app.session_terminal_grants.push("cargo build".to_string());

    assert_eq!(
        app.auto_approval_decision(&ApprovalRequest {
            tool: ApprovalTool::TerminalExec,
            command: Some("FOO=bar cargo build --release".to_string()),
            paths: Vec::new(),
            background: false,
            sandbox_escalation: false,
        }),
        Some(ApprovalDecision::AllowOnce)
    );
}

#[test]
fn edit_grants_do_not_leak_to_prefix_siblings() {
    let mut app = app();
    app.session_edit_grants.push("Cargo.toml".to_string());
    app.session_edit_grants.push("src/".to_string());

    let granted = |app: &App, p: &str| {
        app.auto_approval_decision(&ApprovalRequest {
            tool: ApprovalTool::FileEdit,
            command: None,
            paths: vec![p.to_string()],
            background: false,
            sandbox_escalation: false,
        }) == Some(ApprovalDecision::AllowOnce)
    };

    assert!(granted(&app, "Cargo.toml"));
    assert!(!granted(&app, "Cargo.toml.bak")); // exact-match, no leak
    assert!(granted(&app, "src/main.rs")); // dir subtree
    assert!(!granted(&app, "src-evil/x.rs"));
}

#[test]
fn denied_edits_are_remembered_for_the_turn() {
    let mut app = app();
    let (respond, _decision) = mpsc::channel();
    app.approval_queue.push_back(PendingApproval {
        request: ApprovalRequest {
            tool: ApprovalTool::FileEdit,
            command: None,
            paths: vec!["src/secret.rs".to_string()],
            background: false,
            sandbox_escalation: false,
        },
        respond,
    });
    app.approval_shown_at = Instant::now().checked_sub(APPROVAL_KEY_GRACE * 2);
    app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));

    // A retry of the same edit auto-denies instead of re-prompting.
    assert_eq!(
        app.auto_approval_decision(&ApprovalRequest {
            tool: ApprovalTool::FileEdit,
            command: None,
            paths: vec!["src/secret.rs".to_string()],
            background: false,
            sandbox_escalation: false,
        }),
        Some(ApprovalDecision::Deny)
    );
}

#[test]
fn long_commands_wrap_instead_of_hiding_their_tail() {
    let long = format!("echo {} && rm -rf important", "x".repeat(200));
    let lines = wrap_str(&long, 40);
    assert!(lines.len() > 1);
    // The destructive tail is present somewhere in the wrapped output.
    assert!(lines.iter().any(|line| line.contains("rm -rf important")));
}

#[test]
fn interpreter_grants_are_never_persisted() {
    for command in [
        "bash scripts/lint.sh",
        "python gen.py",
        "sudo rm x",
        "env FOO=1 python evil.py",
        "xargs rm",
    ] {
        assert_eq!(
            derive_terminal_grant_prefix(command),
            None,
            "`{command}` must not yield a persistable grant prefix"
        );
    }
}

#[test]
fn terminal_grant_prefixes_are_derived_conservatively() {
    assert_eq!(
        derive_terminal_grant_prefix("cargo test -p medusa-core"),
        Some("cargo test".to_string())
    );
    assert_eq!(
        derive_terminal_grant_prefix("npm run build --watch"),
        Some("npm run build".to_string())
    );
    assert_eq!(
        derive_terminal_grant_prefix("rustfmt src/main.rs"),
        Some("rustfmt".to_string())
    );
    assert_eq!(
        derive_terminal_grant_prefix("FOO=bar cargo build"),
        Some("cargo build".to_string())
    );
    assert_eq!(derive_terminal_grant_prefix("cargo test && rm -rf /"), None);
    assert_eq!(derive_terminal_grant_prefix("echo hi | sh"), None);
    assert_eq!(derive_terminal_grant_prefix(""), None);
}
