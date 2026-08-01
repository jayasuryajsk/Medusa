use super::*;

#[test]
fn plan_mode_badge_shows_in_composer_title() {
    let mut app = app();

    app.plan_mode = true;
    let title = app.input_title_content();
    let text = title
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>();
    assert!(text.contains(" plan "));
}

#[test]
fn workflow_help_describes_only_scripted_workflows() {
    let mut app = app();

    assert!(app.run_local_tool_command("/workflow"));

    assert!(matches!(
        app.transcript.last(),
        Some(TranscriptItem::Message(ChatMessage {
            role: ChatRole::System,
            content,
            ..
        })) if content.contains("model author and run a task-specific JS workflow")
    ));
}

#[test]
fn workflow_events_create_and_finish_transcript_run() {
    let mut app = app();

    app.apply_workflow_event(WorkflowEvent::RunStarted {
        run_id: "workflow-test".to_string(),
        title: "inspect code".to_string(),
        task: "inspect code".to_string(),
    });
    app.apply_workflow_event(WorkflowEvent::PhaseStarted {
        run_id: "workflow-test".to_string(),
        phase_index: 0,
        name: "scan".to_string(),
        agent_count: 1,
    });
    app.apply_workflow_event(WorkflowEvent::AgentStarted {
        run_id: "workflow-test".to_string(),
        phase_index: 0,
        agent_index: 0,
        name: "mapper".to_string(),
        role: "mapper".to_string(),
        tool_policy: SubagentToolPolicy::ShellRead,
    });
    app.apply_workflow_event(WorkflowEvent::AgentFinished {
        run_id: "workflow-test".to_string(),
        phase_index: 0,
        agent_index: 0,
        name: "mapper".to_string(),
        status: WorkflowStatus::Succeeded,
        output: "found src/main.rs".to_string(),
        tool_counts: BTreeMap::new(),
    });
    let finished = app.apply_workflow_event(WorkflowEvent::RunFinished {
        run_id: "workflow-test".to_string(),
        status: WorkflowStatus::Succeeded,
        summary: "workflow completed".to_string(),
    });

    assert!(finished);
    assert_eq!(app.workflows.len(), 1);
    assert_eq!(app.workflows[0].status, WorkflowViewState::Succeeded);
    assert!(matches!(
        app.transcript.first(),
        Some(TranscriptItem::Workflow(workflow))
            if workflow.id == "workflow-test" && workflow.summary == "workflow completed"
    ));
    assert!(matches!(
        app.transcript.last(),
        Some(TranscriptItem::Message(ChatMessage { role: ChatRole::Assistant, content, .. }))
            if content == "workflow completed"
    ));
}

#[test]
fn script_workflow_events_append_dynamic_phases_and_agents() {
    let mut app = app();

    app.apply_workflow_event(WorkflowEvent::RunStarted {
        run_id: "script-test".to_string(),
        title: "script:bug-hunt".to_string(),
        task: "bug-hunt".to_string(),
    });
    app.apply_workflow_event(WorkflowEvent::PhaseStarted {
        run_id: "script-test".to_string(),
        phase_index: 0,
        name: "find round 1".to_string(),
        agent_count: 0,
    });
    app.apply_workflow_event(WorkflowEvent::AgentStarted {
        run_id: "script-test".to_string(),
        phase_index: 0,
        agent_index: 1,
        name: "finder-2".to_string(),
        role: "finder-2".to_string(),
        tool_policy: SubagentToolPolicy::ShellRead,
    });
    app.apply_workflow_event(WorkflowEvent::AgentFinished {
        run_id: "script-test".to_string(),
        phase_index: 0,
        agent_index: 1,
        name: "finder-2".to_string(),
        status: WorkflowStatus::Succeeded,
        output: "no bugs".to_string(),
        tool_counts: BTreeMap::new(),
    });
    app.apply_workflow_event(WorkflowEvent::Log {
        run_id: "script-test".to_string(),
        message: "round 1: nothing new".to_string(),
    });

    let workflow = &app.workflows[0];
    assert_eq!(workflow.phases.len(), 1);
    assert_eq!(workflow.phases[0].name, "find round 1");
    assert_eq!(workflow.phases[0].agents.len(), 2);
    assert_eq!(workflow.phases[0].agents[1].name, "finder-2");
    assert_eq!(
        workflow.phases[0].agents[1].status,
        WorkflowViewState::Succeeded
    );
    assert!(app.status_line.contains("round 1: nothing new"));
}

#[test]
fn partial_workflow_is_not_rendered_as_total_failure() {
    let mut app = app();

    app.apply_workflow_event(WorkflowEvent::RunStarted {
        run_id: "workflow-partial".to_string(),
        title: "split tui crate".to_string(),
        task: "split tui crate".to_string(),
    });
    app.apply_workflow_event(WorkflowEvent::PhaseStarted {
        run_id: "workflow-partial".to_string(),
        phase_index: 0,
        name: "implementation".to_string(),
        agent_count: 1,
    });
    app.apply_workflow_event(WorkflowEvent::AgentStarted {
        run_id: "workflow-partial".to_string(),
        phase_index: 0,
        agent_index: 0,
        name: "implementer".to_string(),
        role: "implementation agent".to_string(),
        tool_policy: SubagentToolPolicy::Edit,
    });
    app.apply_workflow_event(WorkflowEvent::AgentFinished {
        run_id: "workflow-partial".to_string(),
        phase_index: 0,
        agent_index: 0,
        name: "implementer".to_string(),
        status: WorkflowStatus::Succeeded,
        output: "moved terminal helpers".to_string(),
        tool_counts: BTreeMap::new(),
    });
    app.apply_workflow_event(WorkflowEvent::PhaseStarted {
        run_id: "workflow-partial".to_string(),
        phase_index: 1,
        name: "verification".to_string(),
        agent_count: 1,
    });
    app.apply_workflow_event(WorkflowEvent::AgentStarted {
        run_id: "workflow-partial".to_string(),
        phase_index: 1,
        agent_index: 0,
        name: "verifier".to_string(),
        role: "verification agent".to_string(),
        tool_policy: SubagentToolPolicy::Verify,
    });
    app.apply_workflow_event(WorkflowEvent::AgentFinished {
        run_id: "workflow-partial".to_string(),
        phase_index: 1,
        agent_index: 0,
        name: "verifier".to_string(),
        status: WorkflowStatus::Failed,
        output: "subagent failed: backend overloaded".to_string(),
        tool_counts: BTreeMap::new(),
    });
    let finished = app.apply_workflow_event(WorkflowEvent::RunFinished {
        run_id: "workflow-partial".to_string(),
        status: WorkflowStatus::PartiallySucceeded,
        summary: "workflow partially completed: useful work landed; verifier failed".to_string(),
    });

    assert!(finished);
    assert_eq!(
        app.workflows[0].status,
        WorkflowViewState::PartiallySucceeded
    );
    assert_eq!(app.status_line, "workflow partially complete");
    assert_eq!(
        app.toast.as_ref().map(|toast| toast.kind),
        Some(ToastKind::Warning)
    );
    assert!(matches!(
        app.transcript.last(),
        Some(TranscriptItem::Message(ChatMessage { role: ChatRole::Assistant, content, .. }))
            if content.contains("partially completed")
    ));
}

#[test]
fn active_workflow_does_not_block_model_turn_but_blocks_reload() {
    let mut app = App::with_model_backend(true);
    let (_sender, receiver) = mpsc::channel();
    let workflow = background_workflow(&app, receiver);
    app.workflow_events.push(workflow);

    assert!(!app.is_working());
    assert!(app.has_active_workflows());

    app.start_model_turn("foreground task");
    assert!(app.model_events.is_some());
    assert!(app.queued_turns.is_empty());

    app.model_events = None;
    app.streaming_message = None;
    app.request_reload();

    assert!(!app.should_quit);
    assert_eq!(app.status_line, "reload blocked: work is still running");
}

/// Finding [23]: the real turn-start path must hand the worker a
/// `ToolRuntime` carrying the checkpoint recorder AND the turn's cancel
/// token — deleting any of that wiring must fail this test.
#[test]
fn start_model_turn_wires_recorder_and_cancel_onto_worker_runtime() {
    let mut app = App::with_model_backend(true);
    app.transcript
        .push(TranscriptItem::Message(ChatMessage::user("edit the file")));

    app.start_model_turn("edit the file");

    // App-side wiring (guards `self.active_checkpoint` / `self.turn_cancel`).
    assert!(
        app.active_checkpoint.is_some(),
        "turn must own a checkpoint recorder for finish/prune"
    );
    let turn_cancel = app
        .turn_cancel
        .clone()
        .expect("turn must own a cancel token");

    // Worker-side wiring: the exact runtime handed to the worker must carry
    // the recorder and the SAME cancel token (App-side state cannot prove
    // the `.with_checkpoint_recorder` / `.with_cancel_token` calls).
    let runtime = app
        .last_turn_runtime
        .as_ref()
        .expect("worker runtime should be captured");
    assert!(
        runtime.has_checkpoint_recorder(),
        "worker runtime must carry the checkpoint recorder"
    );
    assert!(!runtime.cancel_token().is_cancelled());
    turn_cancel.cancel();
    assert!(
        runtime.cancel_token().is_cancelled(),
        "worker runtime must share the turn's cancel token"
    );
}

/// Finding [12]: background workflow tools must carry a checkpoint recorder
/// (and a cancel token) so subagent file edits are rewindable.
#[test]
fn saved_workflow_wires_recorder_and_cancel_onto_worker_runtime() {
    let mut app = App::with_model_backend(true);
    write_saved_workflow(&app, "checkpoint-test", "return 'done';");

    app.start_workflow_script("checkpoint-test", "");

    let runtime = app
        .last_workflow_runtime
        .as_ref()
        .expect("workflow worker runtime should be captured");
    assert!(
        runtime.has_checkpoint_recorder(),
        "workflow subagent edits must be checkpointed so /rewind can undo them"
    );
    let workflow = app.workflow_events.last().expect("workflow registered");
    assert!(!workflow.cancel.is_cancelled());
    // A turn cancel stops the background workflow's tools.
    app.finalize_cancelled_turn("interrupted");
    assert!(
        app.workflow_events.iter().all(|w| w.cancel.is_cancelled()),
        "cancel must reach the background workflow so its tools bail"
    );
}

/// Finding [12] end-to-end: a file mutated during a workflow run lands in a
/// checkpoint that `/rewind` can list and restore.
#[test]
fn workflow_file_edits_are_captured_in_a_rewindable_checkpoint() {
    let mut app = App::with_model_backend(true);
    let workspace = app.tools.workspace().to_path_buf();
    fs::write(workspace.join("auth.rs"), "old\n").unwrap();
    write_saved_workflow(&app, "checkpoint-test", "return 'done';");

    app.start_workflow_script("checkpoint-test", "");
    // A subagent edits a file through the run's shared recorder (the
    // worker's ToolRuntime holds a clone of this exact recorder).
    let recorder = app.workflow_events.last().unwrap().checkpoint.clone();
    recorder.capture(&["auth.rs".to_string()]).unwrap();
    fs::write(workspace.join("auth.rs"), "new\n").unwrap();

    let entries = CheckpointStore::open(&workspace)
        .and_then(|store| store.list())
        .unwrap();
    let entry = entries
        .iter()
        .find(|entry| entry.prompt_excerpt == "/workflow checkpoint-test")
        .expect("workflow run must produce a rewindable checkpoint");

    // And it actually restores the pre-edit content.
    CheckpointStore::open(&workspace)
        .and_then(|store| store.restore(&entry.id))
        .unwrap();
    assert_eq!(
        fs::read_to_string(workspace.join("auth.rs")).unwrap(),
        "old\n"
    );
}

#[test]
fn drain_workflow_events_keeps_other_background_jobs_active() {
    let mut app = app();
    let (finished_sender, finished_receiver) = mpsc::channel();
    let (_active_sender, active_receiver) = mpsc::channel();
    finished_sender
        .send(WorkflowEvent::RunStarted {
            run_id: "workflow-test".to_string(),
            title: "inspect code".to_string(),
            task: "inspect code".to_string(),
        })
        .unwrap();
    finished_sender
        .send(WorkflowEvent::RunFinished {
            run_id: "workflow-test".to_string(),
            status: WorkflowStatus::Succeeded,
            summary: "workflow completed".to_string(),
        })
        .unwrap();
    drop(finished_sender);
    let finished = background_workflow(&app, finished_receiver);
    let active = background_workflow(&app, active_receiver);
    app.workflow_events.push(finished);
    app.workflow_events.push(active);

    app.drain_workflow_events();

    assert_eq!(app.workflow_events.len(), 1);
    assert_eq!(app.workflows.len(), 1);
    assert_eq!(app.workflows[0].status, WorkflowViewState::Succeeded);
    assert!(matches!(
        app.transcript.last(),
        Some(TranscriptItem::Message(ChatMessage { role: ChatRole::Assistant, content, .. }))
            if content == "workflow completed"
    ));
}

/// [7]: a turn queued while a background workflow runs must start when the
/// workflow finishes — the only other dequeue site is a model turn's
/// completion, so without this the prompt strands forever.
#[test]
fn finished_background_workflow_starts_queued_turn() {
    let mut app = app(); // model disabled: no worker thread, fully offline
    let (sender, receiver) = mpsc::channel::<WorkflowEvent>();
    app.workflow_events
        .push(background_workflow(&app, receiver));
    app.queued_turns
        .push_back("fix the failing test".to_string());

    // The workflow completes and its channel disconnects.
    sender
        .send(WorkflowEvent::RunFinished {
            run_id: "workflow-test".to_string(),
            status: WorkflowStatus::Succeeded,
            summary: "done".to_string(),
        })
        .unwrap();
    drop(sender);

    app.drain_workflow_events();

    assert!(
        app.queued_turns.is_empty(),
        "workflow completion must drain the queued turn"
    );
    assert!(
        app.transcript.iter().any(|item| matches!(
            item,
            TranscriptItem::Message(ChatMessage { role: ChatRole::User, content, .. })
                if content == "fix the failing test"
        )),
        "the queued turn must actually be started (its user message appended)"
    );
}

#[test]
fn workflow_rows_render_phase_tree() {
    let workflow = WorkflowRunView {
        id: "workflow-test".to_string(),
        title: "audit auth".to_string(),
        task: "audit auth".to_string(),
        status: WorkflowViewState::Running,
        phases: vec![WorkflowPhaseView {
            name: "recon".to_string(),
            objective: "Map code".to_string(),
            status: WorkflowViewState::Running,
            agents: vec![WorkflowAgentView {
                name: "mapper".to_string(),
                role: "mapper".to_string(),
                tool_policy: SubagentToolPolicy::ShellRead,
                status: WorkflowViewState::Running,
                output: String::new(),
                tool_counts: BTreeMap::new(),
            }],
        }],
        summary: String::new(),
        expanded: false,
    };
    let transcript = vec![TranscriptItem::Workflow(workflow)];

    let lines = visible_transcript_lines(&transcript, None, None);
    let text = lines.iter().map(line_text).collect::<Vec<_>>();

    assert!(text.iter().any(|line| line.contains("workflow")));
    assert!(text.iter().any(|line| line.contains("audit auth")));
    assert!(text.iter().any(|line| line.contains("recon")));
    assert!(text.iter().any(|line| line.contains("mapper")));
    assert!(text.iter().any(|line| line.contains("[shell-read]")));
    assert!(text.iter().any(|line| line.contains("running")));
}

#[test]
fn empty_transcript_renders_launch_masthead() {
    let transcript = Vec::new();
    let lines = visible_transcript_lines(&transcript, None, None);
    let text = lines.iter().map(line_text).collect::<Vec<_>>();

    // Wordmark is width-adaptive: wide ansi-shadow or compact fallback.
    assert!(
        text.iter()
            .any(|line| line.contains("███╗") || line.contains("█▀▄▀█"))
    );
    assert!(
        text.iter()
            .any(|line| line.contains("plans, edits, and verifies"))
    );
    assert!(text.iter().any(|line| line.contains("shift+tab")));
    assert!(text.iter().any(|line| line.contains("ctrl+p")));
    assert!(text.iter().any(|line| line.contains("esc esc")));
}

#[test]
fn first_pending_turn_keeps_launch_masthead_visible() {
    let transcript = vec![
        TranscriptItem::Message(ChatMessage::user("hi")),
        TranscriptItem::Message(ChatMessage::assistant("")),
    ];
    let lines = visible_transcript_lines(&transcript, Some(1), None);
    let text = lines.iter().map(line_text).collect::<Vec<_>>();

    assert!(
        text.iter()
            .any(|line| line.contains("plans, edits, and verifies"))
    );
    assert!(text.iter().any(|line| line.contains("› hi")));
}

#[test]
fn toast_renders_in_status_line() {
    let mut app = app();

    app.toast("Session cleared", ToastKind::Warning);

    let text = line_text(&app.status_line_content());
    assert!(text.contains("warning"));
    assert!(text.contains("Session cleared"));
}

#[test]
fn tree_command_opens_session_tree_modal() {
    let mut app = app();

    app.input = "/tree".to_string();
    app.input_cursor = app.input_len();
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    assert_eq!(app.status_line, "session tree opened");
    assert_eq!(app.active_modal, Some(Modal::SessionTree));
}

#[test]
fn session_fork_writes_parent_metadata_and_updates_pointer() {
    let workspace = temp_workspace();
    let mut session = SessionStore::open(&workspace, SessionOpenMode::New).unwrap();
    let root_id = session.current_id();
    let transcript = vec![TranscriptItem::Message(ChatMessage::user("root task"))];

    session.save_transcript(&transcript).unwrap();
    let fork_id = session.fork(&transcript).unwrap();

    assert_ne!(fork_id, root_id);
    assert_eq!(
        fs::read_to_string(workspace.join(".medusa/sessions/last")).unwrap(),
        fork_id
    );

    let fork_file = read_session_file::<TranscriptItem, ChatMessage>(
        &workspace.join(".medusa/sessions").join(&fork_id),
    )
    .expect("fork session file should parse");
    assert_eq!(fork_file.session_id.as_deref(), Some(fork_id.as_str()));
    assert_eq!(fork_file.parent_id.as_deref(), Some(root_id.as_str()));
    assert_eq!(fork_file.transcript, transcript);

    let sessions = session.list_sessions();
    let fork = sessions
        .iter()
        .find(|info| info.name == fork_id)
        .expect("fork should be listed");
    assert_eq!(fork.parent, compact_session_id(&root_id));
    assert_eq!(fork.current, "yes");
}

#[test]
fn startup_parser_accepts_named_continue() {
    let args = vec!["continue".to_string(), "session-123.json".to_string()];

    assert_eq!(
        parse_startup_command(&args).unwrap(),
        StartupCommand::Tui(SessionOpenMode::ContinueNamed(
            "session-123.json".to_string()
        ))
    );
}

#[test]
fn startup_parser_returns_help_and_version_without_exiting() {
    assert_eq!(
        parse_startup_command(&["--help".to_string()]).unwrap(),
        StartupCommand::Print(HELP_TEXT)
    );
    assert_eq!(
        parse_startup_command(&["--version".to_string()]).unwrap(),
        StartupCommand::Print(VERSION_TEXT)
    );
    assert!(VERSION_TEXT.starts_with("medusa "));
}

#[test]
fn startup_parser_returns_headless_help_without_exiting() {
    assert_eq!(
        parse_startup_command(&["run".to_string(), "--help".to_string()]).unwrap(),
        StartupCommand::Print(RUN_HELP_TEXT)
    );
}

#[test]
fn startup_parser_accepts_provider_auth_commands() {
    assert_eq!(
        parse_startup_command(&["auth".to_string(), "list".to_string()]).unwrap(),
        StartupCommand::Auth(AuthCommand::List)
    );
    assert_eq!(
        parse_startup_command(&[
            "auth".to_string(),
            "set".to_string(),
            "openai-compatible".to_string(),
        ])
        .unwrap(),
        StartupCommand::Auth(AuthCommand::Set("openai".to_string()))
    );
}

#[test]
fn startup_parser_accepts_headless_run_options() {
    let args = vec![
        "run".to_string(),
        "--model".to_string(),
        "gpt-test".to_string(),
        "--permission".to_string(),
        "readonly".to_string(),
        "--json".to_string(),
        "--".to_string(),
        "fix".to_string(),
        "tests".to_string(),
    ];

    assert_eq!(
        parse_startup_command(&args).unwrap(),
        StartupCommand::Headless(HeadlessOptions {
            task: Some("fix tests".to_string()),
            model: Some("gpt-test".to_string()),
            permission_mode: Some(PermissionMode::Readonly),
            json: true,
            stream: false,
        })
    );
}

#[test]
fn startup_parser_allows_headless_run_task_from_stdin() {
    let args = vec!["run".to_string(), "--no-stream".to_string()];

    assert_eq!(
        parse_startup_command(&args).unwrap(),
        StartupCommand::Headless(HeadlessOptions {
            task: None,
            model: None,
            permission_mode: None,
            json: false,
            stream: false,
        })
    );
}

#[test]
fn session_name_rejects_traversal() {
    assert!(normalize_session_name("../session-1").is_err());
    assert!(normalize_session_name("nested/session-1").is_err());
    assert!(normalize_session_name("last").is_err());
}

#[test]
fn named_session_open_loads_requested_transcript() {
    let workspace = temp_workspace();
    let mut session = SessionStore::open(&workspace, SessionOpenMode::New).unwrap();
    let root_id = session.current_id();
    let root_transcript = vec![TranscriptItem::Message(ChatMessage::user("root task"))];
    let fork_transcript = vec![TranscriptItem::Message(ChatMessage::user("fork task"))];

    session.save_transcript(&root_transcript).unwrap();
    let fork_id = session.fork(&fork_transcript).unwrap();
    assert_ne!(fork_id, root_id);

    let named = SessionStore::open(
        &workspace,
        SessionOpenMode::ContinueNamed(root_id.trim_end_matches(".json").to_string()),
    )
    .unwrap();

    assert_eq!(named.current_id(), root_id);
    assert_eq!(named.load_transcript().unwrap(), root_transcript);
    assert_eq!(
        fs::read_to_string(workspace.join(".medusa/sessions/last")).unwrap(),
        root_id
    );
}

#[test]
fn fork_command_switches_active_session() {
    let workspace = temp_workspace();
    let session = SessionStore::open(&workspace, SessionOpenMode::New).unwrap();
    let mut app = App::build(false, Some(session));
    app.transcript
        .push(TranscriptItem::Message(ChatMessage::user("try one path")));

    app.input = "/fork".to_string();
    app.input_cursor = app.input_len();
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    assert!(app.status_line.starts_with("forked session-"));
    assert!(app.session.as_ref().unwrap().parent_id().is_some());
    assert_eq!(app.transcript.len(), 1);
}

#[test]
fn resume_command_switches_active_session() {
    let workspace = temp_workspace();
    let mut session = SessionStore::open(&workspace, SessionOpenMode::New).unwrap();
    let root_id = session.current_id();
    let root_transcript = vec![TranscriptItem::Message(ChatMessage::user("root task"))];
    let fork_transcript = vec![TranscriptItem::Message(ChatMessage::user("fork task"))];

    session.save_transcript(&root_transcript).unwrap();
    session.fork(&fork_transcript).unwrap();
    let mut app = App::build(false, Some(session));
    app.transcript = fork_transcript;

    app.input = format!("/resume {}", root_id.trim_end_matches(".json"));
    app.input_cursor = app.input_len();
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    assert_eq!(app.status_line, format!("resumed {root_id}"));
    assert_eq!(app.transcript, root_transcript);
    assert_eq!(app.session.as_ref().unwrap().current_id(), root_id);
    assert_eq!(
        fs::read_to_string(workspace.join(".medusa/sessions/last")).unwrap(),
        root_id
    );
}
