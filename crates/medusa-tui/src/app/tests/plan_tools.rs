use super::*;

#[test]
fn removed_viewer_commands_report_unknown() {
    let mut app = app();

    for command in ["/images", "/themes", "/demo", "/recap"] {
        assert!(app.run_local_tool_command(command));
    }
    assert!(matches!(
        app.transcript.last(),
        Some(TranscriptItem::Message(ChatMessage { content, .. }))
            if content.contains("unknown command: /recap")
    ));
}

#[test]
fn plan_updates_render_in_strip_not_transcript() {
    let mut app = app();
    app.apply_plan_update_output(
        r#"{"summary":"Ship plan UI","items":[{"text":"Inspect current renderer","status":"done","evidence":["main.rs"]},{"text":"Render plan rows","status":"active"},{"text":"Run tests","status":"pending"}]}"#,
    )
    .unwrap();

    let Some(plan) = app.current_plan() else {
        panic!("expected current plan");
    };
    assert_eq!(plan.summary, "Ship plan UI");
    assert_eq!(plan.items[1].status, PlanItemStatus::Active);

    // Not in the chat transcript…
    let lines = visible_transcript_lines(&app.transcript, None, None);
    let text = lines.iter().map(line_text).collect::<Vec<_>>();
    assert!(!text.iter().any(|line| line.contains("Render plan rows")));

    // …but in the strip above the composer.
    let strip = plan_strip_lines(app.plan_strip().expect("strip visible"))
        .iter()
        .map(line_text)
        .collect::<Vec<_>>();
    assert!(strip[0].contains("plan · 1/3"));
    assert!(strip[0].contains("Ship plan UI"));
    assert!(strip.iter().any(|line| line.contains("Render plan rows")));
    assert!(app.plan_strip_height(40) > 0);
}

#[test]
fn completed_plan_leaves_the_strip() {
    let mut app = app();
    app.apply_plan_update_output(
        r#"{"summary":"Done","items":[{"text":"one","status":"done"},{"text":"two","status":"done"}]}"#,
    )
    .unwrap();

    assert!(app.plan_strip().is_none());
    assert_eq!(app.plan_strip_height(40), 0);
}

#[test]
fn long_plan_strip_folds_completed_prefix_and_tail() {
    let mut app = app();
    let items = (1..=12)
        .map(|index| {
            let status = if index <= 4 {
                "done"
            } else if index == 5 {
                "active"
            } else {
                "pending"
            };
            format!(r#"{{"text":"step {index}","status":"{status}"}}"#)
        })
        .collect::<Vec<_>>()
        .join(",");
    app.apply_plan_update_output(&format!(r#"{{"summary":"Big","items":[{items}]}}"#))
        .unwrap();

    let strip = plan_strip_lines(app.plan_strip().expect("strip visible"))
        .iter()
        .map(line_text)
        .collect::<Vec<_>>();
    assert!(strip.iter().any(|line| line.contains("✓ 4 done")));
    assert!(strip.iter().any(|line| line.contains("step 5")));
    assert!(!strip.iter().any(|line| line.contains("step 1 ")));
    assert!(
        strip.last().unwrap().contains("… "),
        "tail folds: {strip:?}"
    );
}

#[test]
fn consecutive_plan_updates_replace_latest_snapshot() {
    let mut app = app();

    app.apply_plan_update_output(
        r#"{"summary":"First","items":[{"text":"one","status":"active"}]}"#,
    )
    .unwrap();
    app.apply_plan_update_output(
        r#"{"summary":"Second","items":[{"text":"one","status":"done"},{"text":"two","status":"active","evidence":["cargo check"]}]}"#,
    )
    .unwrap();

    assert_eq!(app.transcript.len(), 1);
    let Some(plan) = app.current_plan() else {
        panic!("expected current plan");
    };
    assert_eq!(plan.summary, "Second");
    assert_eq!(plan.items[0].status, PlanItemStatus::Done);
    assert!(
        plan.items[1]
            .evidence
            .iter()
            .any(|line| line.contains("cargo check"))
    );
}

#[test]
fn decision_request_output_renders_inline() {
    let mut app = app();

    app.apply_decision_request_output(
        r#"{"title":"Choose storage","reason":"Storage changes how the plan is implemented.","questions":[{"id":"storage","prompt":"Where should plans live?","kind":"choice","options":["transcript","file"],"recommended":"transcript","required":true}],"assumptions":["Use transcript if the user does not care."]}"#,
    )
    .unwrap();

    let Some(decision) = app.pending_decision() else {
        panic!("expected pending decision");
    };
    assert_eq!(decision.title, "Choose storage");
    assert_eq!(decision.questions.len(), 1);
    assert_eq!(decision.questions[0].kind, DecisionQuestionKind::Choice);

    let lines = visible_transcript_lines(&app.transcript, None, None);
    let text = lines.iter().map(line_text).collect::<Vec<_>>();
    assert!(text.iter().any(|line| line.contains("decision")));
    assert!(text.iter().any(|line| line.contains("waiting")));
    assert!(
        text.iter()
            .any(|line| line.contains("Where should plans live?"))
    );
    assert!(text.iter().any(|line| line.contains("Choose storage")));
    assert!(text.iter().any(|line| line.contains("transcript")));
}

#[test]
fn answering_pending_decision_marks_it_answered() {
    let mut app = app();
    app.apply_decision_request_output(
        r#"{"title":"Pick approach","questions":[{"id":"approach","prompt":"Which path?","kind":"text","options":[],"required":true}]}"#,
    )
    .unwrap();

    app.input = "Use the simple transcript path.".to_string();
    app.input_cursor = app.input_len();
    app.submit_input();

    let Some(decision) = app.current_decision() else {
        panic!("expected decision");
    };
    assert!(decision.answered);
    assert_eq!(
        decision.answers.get("approach").map(String::as_str),
        Some("Use the simple transcript path.")
    );
    assert!(
        decision
            .answer
            .as_deref()
            .is_some_and(|answer| answer.contains("- approach: Use the simple transcript path."))
    );
    assert_eq!(
        app.toast.as_ref().map(|toast| toast.kind),
        Some(ToastKind::Success)
    );
}

#[test]
fn decision_choice_can_be_selected_with_keyboard_and_submitted() {
    let mut app = app();
    app.apply_decision_request_output(
        r#"{"title":"Semantic indexing","questions":[{"id":"embedding","prompt":"Which embedding approach?","kind":"choice","options":["local embeddings","remote API embeddings","hybrid"],"recommended":"local embeddings","required":true},{"id":"timing","prompt":"When should it run?","kind":"choice","options":["manual only","lazy on first search","background on startup"],"recommended":"lazy on first search","required":true}]}"#,
    )
    .unwrap();

    app.handle_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE));
    assert_eq!(
        app.pending_decision()
            .and_then(|decision| decision.answers.get("embedding"))
            .map(String::as_str),
        Some("remote API embeddings")
    );

    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    let Some(decision) = app.current_decision() else {
        panic!("expected decision");
    };
    assert!(decision.answered);
    assert_eq!(
        decision.answers.get("timing").map(String::as_str),
        Some("background on startup")
    );
    assert!(matches!(
        app.transcript.last(),
        Some(TranscriptItem::Message(ChatMessage { role: ChatRole::User, content, .. }))
            if content.contains("Decision answer: Semantic indexing")
                && content.contains("- embedding: remote API embeddings")
                && content.contains("- timing: background on startup")
    ));
}

#[test]
fn text_decision_questions_do_not_steal_regular_typing_keys() {
    let mut app = app();
    app.apply_decision_request_output(
        r#"{"title":"Name workflow","questions":[{"id":"name","prompt":"What should it be called?","kind":"text","options":[],"required":true}]}"#,
    )
    .unwrap();

    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE));

    assert_eq!(app.input, "jit");
    assert_eq!(app.decision_selection, 0);
    assert_eq!(app.selected_tool, None);
}

#[test]
fn tool_output_failure_detects_nonzero_exit() {
    assert!(!tool_output_failed("exit: 0\nstdout:\nok"));
    assert!(tool_output_failed("exit: 101\nstderr:\nfailed"));
    assert!(tool_output_failed("error: nope"));
}

#[test]
fn visible_tool_activity_lines_show_running_state() {
    let transcript = vec![TranscriptItem::Tool(ToolRun {
        id: None,
        started_at: Instant::now(),
        pending_result: None,
        name: "file.patch".to_string(),
        summary: "apply patch".to_string(),
        state: ToolRunState::Running,
        detail: String::new(),
        expanded: false,
        group_expanded: false,
    })];

    let lines = visible_transcript_lines(&transcript, None, None);
    let text = lines.iter().map(line_text).collect::<Vec<_>>();

    assert!(text[0].contains("patch apply patch"));
    assert!(
        text[0].contains("⠁")
            || text[0].contains("⠃")
            || text[0].contains("⠇")
            || text[0].contains("⠧")
            || text[0].contains("⠷")
            || text[0].contains("⠿")
    );
    assert!(text[1].contains("⎿ running…"));
}

#[test]
fn running_tool_pulse_animation_changes_braille_only() {
    let run = ToolRun {
        id: None,
        started_at: Instant::now(),
        pending_result: None,
        name: "terminal.exec".to_string(),
        summary: "$ cargo test".to_string(),
        state: ToolRunState::Running,
        detail: String::new(),
        expanded: false,
        group_expanded: false,
    };

    let mut first = Vec::new();
    append_tool_call_lines(
        &mut first,
        &run,
        false,
        RenderContext {
            animation_tick: 0,
            ..Default::default()
        },
    );
    let mut second = Vec::new();
    append_tool_call_lines(
        &mut second,
        &run,
        false,
        RenderContext {
            animation_tick: 12,
            ..Default::default()
        },
    );

    let first_text = first.iter().map(line_text).collect::<Vec<_>>();
    let second_text = second.iter().map(line_text).collect::<Vec<_>>();

    assert_ne!(first_text[0], second_text[0]);
    assert_eq!(first_text[1], second_text[1]);
    assert!(!second_text[0].contains("⬤"));
    assert!(second_text[0].contains("⠧"));
    assert!(second_text[1].contains("running"));
}

#[test]
fn tool_calls_render_one_block_per_call() {
    let read = ToolRun {
        id: None,
        started_at: Instant::now(),
        pending_result: None,
        name: "file.read".to_string(),
        summary: "crates/medusa-tui/src/main.rs".to_string(),
        state: ToolRunState::Succeeded,
        detail: "read 1 • crates/medusa-tui/src/main.rs".to_string(),
        expanded: false,
        group_expanded: false,
    };
    let failed_patch = ToolRun {
        id: None,
        started_at: Instant::now(),
        pending_result: None,
        name: "file.patch".to_string(),
        summary: "apply patch".to_string(),
        state: ToolRunState::Failed,
        detail: "error: patch rejected\ncontext mismatch at line 4".to_string(),
        expanded: false,
        group_expanded: false,
    };
    let transcript = vec![
        TranscriptItem::Tool(read),
        TranscriptItem::Tool(failed_patch),
    ];

    let mut lines = Vec::new();
    append_tool_group_lines(
        &mut lines,
        &transcript,
        0,
        transcript.len(),
        None,
        RenderContext::static_view(),
    );
    let text = lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");

    assert!(text.contains("• read crates/medusa-tui/src/main.rs"));
    assert!(text.contains("⎿ read 1 • crates/medusa-tui/src/main.rs"));
    assert!(text.contains("• patch apply patch"));
    assert!(text.contains("⎿ error: patch rejected"));
    assert!(text.contains("context mismatch at line 4"));
}

#[test]
fn collapsed_tool_output_shows_expand_hint() {
    let noisy = ToolRun {
        id: None,
        started_at: Instant::now(),
        pending_result: None,
        name: "terminal.exec".to_string(),
        summary: "$ cargo test".to_string(),
        state: ToolRunState::Succeeded,
        detail: (1..=6)
            .map(|index| format!("output line {index}"))
            .collect::<Vec<_>>()
            .join("\n"),
        expanded: false,
        group_expanded: false,
    };

    let mut lines = Vec::new();
    append_tool_call_lines(&mut lines, &noisy, false, RenderContext::static_view());
    let text = lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");

    assert!(text.contains("⎿ output line 1"));
    assert!(!text.contains("output line 2"));
    assert!(text.contains("+5 lines (enter to expand)"));

    let mut expanded_run = noisy;
    expanded_run.expanded = true;
    let mut expanded_lines = Vec::new();
    append_tool_call_lines(
        &mut expanded_lines,
        &expanded_run,
        false,
        RenderContext::static_view(),
    );
    let expanded_text = expanded_lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(expanded_text.contains("output line 6"));
    assert!(!expanded_text.contains("enter to expand"));
}

#[test]
fn consecutive_tool_rows_render_as_one_activity_block() {
    let transcript = vec![
        TranscriptItem::Tool(ToolRun {
            id: None,
            started_at: Instant::now(),
            pending_result: None,
            name: "terminal.exec".to_string(),
            summary: "$ cargo test -p medusa-tui".to_string(),
            state: ToolRunState::Succeeded,
            detail: "24 passed".to_string(),
            expanded: false,
            group_expanded: false,
        }),
        TranscriptItem::Tool(ToolRun {
            id: None,
            started_at: Instant::now(),
            pending_result: None,
            name: "file.patch".to_string(),
            summary: "crates/medusa-tui/src/main.rs - update renderer".to_string(),
            state: ToolRunState::Failed,
            detail: "error: patch rejected\nrecovery: inspect context".to_string(),
            expanded: false,
            group_expanded: false,
        }),
    ];

    let lines = visible_transcript_lines(&transcript, None, None);
    let text = lines.iter().map(line_text).collect::<Vec<_>>();

    assert!(
        text.iter()
            .any(|line| line.contains("terminal $ cargo test -p medusa-tui"))
    );
    assert!(text.iter().any(|line| line.contains("⎿ 24 passed")));
    assert!(
        text.iter()
            .any(|line| line.contains("patch crates/medusa-tui/src/main.rs - update renderer"))
    );
    assert!(
        text.iter()
            .any(|line| line.contains("⎿ error: patch rejected"))
    );
    assert!(
        text.iter()
            .any(|line| line.contains("recovery: inspect context"))
    );
}

fn finished_tool(name: &str, summary: &str, state: ToolRunState) -> ToolRun {
    ToolRun {
        id: None,
        started_at: Instant::now(),
        pending_result: None,
        name: name.to_string(),
        summary: summary.to_string(),
        state,
        detail: "done".to_string(),
        expanded: false,
        group_expanded: false,
    }
}

#[test]
fn consecutive_same_tool_calls_coalesce_into_one_line() {
    let transcript = vec![
        TranscriptItem::Tool(finished_tool(
            "file.read",
            "read src/main.rs",
            ToolRunState::Succeeded,
        )),
        TranscriptItem::Tool(finished_tool(
            "file.read",
            "read src/tools.rs",
            ToolRunState::Succeeded,
        )),
        TranscriptItem::Tool(finished_tool(
            "file.read",
            "read src/wire.rs",
            ToolRunState::Succeeded,
        )),
        TranscriptItem::Tool(finished_tool(
            "file.read",
            "read src/exec.rs",
            ToolRunState::Succeeded,
        )),
        TranscriptItem::Tool(finished_tool(
            "terminal.exec",
            "$ cargo check",
            ToolRunState::Succeeded,
        )),
    ];

    let lines = visible_transcript_lines(&transcript, None, None);
    let text = lines.iter().map(line_text).collect::<Vec<_>>();

    assert!(
        text.iter()
            .any(|line| { line.contains("read src/main.rs, src/tools.rs, src/wire.rs +1 more") })
    );
    assert!(text.iter().any(|line| line.contains("⎿ 4 calls")));
    // The lone terminal call renders as a normal block.
    assert!(
        text.iter()
            .any(|line| line.contains("terminal $ cargo check"))
    );
    assert!(!text.iter().any(|line| line.contains("read src/tools.rs\n")));
}

#[test]
fn out_of_order_tool_results_land_on_the_right_blocks_by_call_id() {
    let mut app = app();
    app.push_tool_start_with_id(
        Some("call_a".to_string()),
        "file.read".to_string(),
        "read src/a.rs".to_string(),
    );
    app.push_tool_start_with_id(
        Some("call_b".to_string()),
        "file.read".to_string(),
        "read src/b.rs".to_string(),
    );
    for item in &mut app.transcript {
        if let TranscriptItem::Tool(run) = item {
            run.started_at = run
                .started_at
                .checked_sub(MIN_TOOL_PULSE_VISIBLE)
                .unwrap_or(run.started_at);
        }
    }

    // Second call finishes first (parallel execution), then the first.
    app.push_tool_result_for_call("call_b", "file.read", "content of b".to_string());
    app.push_tool_result_for_call("call_a", "file.read", "content of a".to_string());

    let TranscriptItem::Tool(first) = &app.transcript[0] else {
        panic!("expected tool run");
    };
    let TranscriptItem::Tool(second) = &app.transcript[1] else {
        panic!("expected tool run");
    };
    assert_eq!(first.detail, "content of a");
    assert_eq!(second.detail, "content of b");
    assert_eq!(first.state, ToolRunState::Succeeded);
    assert_eq!(second.state, ToolRunState::Succeeded);
}

#[test]
fn edit_tool_shows_diff_lines_and_never_coalesces() {
    let mut first = finished_tool("file.edit", "edit src/a.rs", ToolRunState::Succeeded);
    first.detail =
        "edited src/a.rs (1 replacement)\n- fn old() {}\n+ fn new() {}\n  shared".to_string();
    let mut second = finished_tool("file.edit", "edit src/b.rs", ToolRunState::Succeeded);
    second.detail = "edited src/b.rs (1 replacement)\n- x\n+ y".to_string();
    let transcript = vec![TranscriptItem::Tool(first), TranscriptItem::Tool(second)];

    let lines = visible_transcript_lines(&transcript, None, None);
    let text = lines.iter().map(line_text).collect::<Vec<_>>();

    // Both edits stay as separate blocks with their diff bodies visible.
    assert!(text.iter().any(|line| line.contains("edit src/a.rs")));
    assert!(text.iter().any(|line| line.contains("edit src/b.rs")));
    assert!(text.iter().any(|line| line.contains("- fn old() {}")));
    assert!(text.iter().any(|line| line.contains("+ fn new() {}")));
    assert!(!text.iter().any(|line| line.contains("2 calls")));
}

#[test]
fn running_call_joins_coalesced_run_as_live_tail() {
    let mut running = finished_tool("file.read", "read src/slow.rs", ToolRunState::Running);
    running.detail = String::new();
    let transcript = vec![
        TranscriptItem::Tool(finished_tool(
            "file.read",
            "read src/main.rs",
            ToolRunState::Succeeded,
        )),
        TranscriptItem::Tool(finished_tool(
            "file.read",
            "read src/tools.rs",
            ToolRunState::Succeeded,
        )),
        TranscriptItem::Tool(running),
    ];

    let lines = visible_transcript_lines(&transcript, None, None);
    let text = lines.iter().map(line_text).collect::<Vec<_>>();

    assert!(
        text.iter()
            .any(|line| line.contains("read src/main.rs, src/tools.rs, src/slow.rs")),
        "running call renders inside the coalesced line, not below it"
    );
    assert!(
        text.iter()
            .any(|line| line.contains("⎿ 3 calls · running…"))
    );
    assert_eq!(
        text.iter()
            .filter(|line| line.contains("src/slow.rs"))
            .count(),
        1,
        "the running call must not also render as its own block"
    );
}

#[test]
fn failed_and_running_calls_never_coalesce() {
    let mut running = finished_tool("file.read", "read src/slow.rs", ToolRunState::Running);
    running.detail = String::new();
    let transcript = vec![
        TranscriptItem::Tool(finished_tool(
            "file.read",
            "read src/main.rs",
            ToolRunState::Succeeded,
        )),
        TranscriptItem::Tool(finished_tool(
            "file.read",
            "read src/missing.rs",
            ToolRunState::Failed,
        )),
        TranscriptItem::Tool(running),
    ];

    let lines = visible_transcript_lines(&transcript, None, None);
    let text = lines.iter().map(line_text).collect::<Vec<_>>();

    assert!(!text.iter().any(|line| line.contains("calls")));
    assert!(text.iter().any(|line| line.contains("read src/main.rs")));
    assert!(text.iter().any(|line| line.contains("read src/missing.rs")));
    assert!(text.iter().any(|line| line.contains("read src/slow.rs")));
}

#[test]
fn reasoning_between_same_tool_calls_does_not_break_coalescing() {
    let transcript = vec![
        TranscriptItem::Tool(finished_tool(
            "file.read",
            "read src/main.rs",
            ToolRunState::Succeeded,
        )),
        TranscriptItem::Reasoning(ReasoningTrace {
            content: "Reading the next file.".to_string(),
            expanded: false,
        }),
        TranscriptItem::Tool(finished_tool(
            "file.read",
            "read src/tools.rs",
            ToolRunState::Succeeded,
        )),
    ];

    let lines = visible_transcript_lines(&transcript, None, None);
    let text = lines.iter().map(line_text).collect::<Vec<_>>();

    assert!(
        text.iter()
            .any(|line| line.contains("read src/main.rs, src/tools.rs"))
    );
    assert!(text.iter().any(|line| line.contains("⎿ 2 calls")));
}

#[test]
fn enter_cycles_coalesced_group_open_then_details_then_coalesced() {
    let mut app = app();
    app.transcript.push(TranscriptItem::Tool(finished_tool(
        "file.read",
        "read src/main.rs",
        ToolRunState::Succeeded,
    )));
    app.transcript.push(TranscriptItem::Tool(finished_tool(
        "file.read",
        "read src/tools.rs",
        ToolRunState::Succeeded,
    )));
    app.transcript.push(TranscriptItem::Tool(finished_tool(
        "terminal.exec",
        "$ cargo check",
        ToolRunState::Succeeded,
    )));

    app.selected_tool = Some(0);
    app.toggle_selected_tool();
    assert!(tool_group_is_open(&app.transcript, 0, 3));
    assert!(
        !matches!(&app.transcript[0], TranscriptItem::Tool(run) if run.expanded),
        "first enter un-coalesces the group without opening details"
    );

    app.toggle_selected_tool();
    assert!(matches!(&app.transcript[0], TranscriptItem::Tool(run) if run.expanded));

    app.toggle_selected_tool();
    assert!(!tool_group_is_open(&app.transcript, 0, 3));
    assert!(!matches!(&app.transcript[0], TranscriptItem::Tool(run) if run.expanded));
}

#[test]
fn interleaved_reasoning_and_tools_render_as_one_batch() {
    let transcript = vec![
        TranscriptItem::Reasoning(ReasoningTrace {
            content: "detailed Inspecting codebase.".to_string(),
            expanded: false,
        }),
        TranscriptItem::Tool(ToolRun {
            id: None,
            started_at: Instant::now(),
            pending_result: None,
            name: "terminal.exec".to_string(),
            summary: "$ rg TODO".to_string(),
            state: ToolRunState::Succeeded,
            detail: "done".to_string(),
            expanded: false,
            group_expanded: false,
        }),
        TranscriptItem::Reasoning(ReasoningTrace {
            content: "Reading matching files.".to_string(),
            expanded: false,
        }),
        TranscriptItem::Tool(ToolRun {
            id: None,
            started_at: Instant::now(),
            pending_result: None,
            name: "terminal.exec".to_string(),
            summary: "$ sed -n '1,80p' README.md".to_string(),
            state: ToolRunState::Succeeded,
            detail: "done".to_string(),
            expanded: false,
            group_expanded: false,
        }),
    ];

    let lines = visible_transcript_lines(&transcript, None, None);
    let text = lines.iter().map(line_text).collect::<Vec<_>>();

    assert!(
        text.iter()
            .any(|line| line.contains("terminal $ rg TODO, $ sed -n '1,80p' README.md"))
    );
    assert!(
        text.iter()
            .any(|line| line.contains("thinking") && line.contains("Reading matching files"))
    );
}

#[test]
fn selected_tool_row_still_stays_collapsed_until_opened() {
    let transcript = vec![TranscriptItem::Tool(ToolRun {
        id: None,
        started_at: Instant::now(),
        pending_result: None,
        name: "terminal.exec".to_string(),
        summary: "$ cargo check".to_string(),
        state: ToolRunState::Succeeded,
        detail: "done".to_string(),
        expanded: false,
        group_expanded: false,
    })];

    let lines = visible_transcript_lines(&transcript, None, Some(0));

    let text = lines.iter().map(line_text).collect::<Vec<_>>();
    assert!(text[0].contains("terminal $ cargo check"));
    assert!(text[1].contains("⎿ done"));
}

#[test]
fn tool_selection_enter_and_close_updates_inline_output_state() {
    let mut app = app();
    app.transcript.push(TranscriptItem::Tool(ToolRun {
        id: None,
        started_at: Instant::now(),
        pending_result: None,
        name: "terminal.exec".to_string(),
        summary: "$ cargo check".to_string(),
        state: ToolRunState::Succeeded,
        detail: "24 passed".to_string(),
        expanded: false,
        group_expanded: false,
    }));

    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
    assert_eq!(app.selected_tool, Some(0));

    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let TranscriptItem::Tool(run) = &app.transcript[0] else {
        panic!("expected tool run");
    };
    assert!(run.expanded);

    app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
    assert_eq!(app.selected_tool, None);
    let TranscriptItem::Tool(run) = &app.transcript[0] else {
        panic!("expected tool run");
    };
    assert!(!run.expanded);
}

#[test]
fn open_selected_tool_renders_inline_output() {
    let transcript = vec![TranscriptItem::Tool(ToolRun {
        id: None,
        started_at: Instant::now(),
        pending_result: None,
        name: "terminal.exec".to_string(),
        summary: "$ cargo test -p medusa-tui".to_string(),
        state: ToolRunState::Succeeded,
        detail: "29 passed\n2 ignored".to_string(),
        expanded: true,
        group_expanded: false,
    })];

    let lines = visible_transcript_lines(&transcript, None, Some(0));

    let text = lines.iter().map(line_text).collect::<Vec<_>>();
    assert!(text.iter().any(|line| line.contains("⎿ 29 passed")));
    assert!(text.iter().any(|line| line.contains("2 ignored")));
}
