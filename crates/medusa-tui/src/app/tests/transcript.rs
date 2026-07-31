use super::*;

#[test]
fn input_title_shows_model_name_when_idle() {
    let app = app();

    let title = app.input_title_content();
    let text = title
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>();

    assert!(text.contains(app.model.model_name()));
    assert!(!text.contains("━"));
}

#[test]
fn input_title_uses_light_sweep_while_working() {
    let mut app = app();

    app.animation_tick = 0;
    app.streaming_message = Some(0);
    let title = app.input_title_content();
    let text = title
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>();

    assert!(text.contains("━"));
    assert!(!text.to_lowercase().contains("message"));
}

#[test]
fn input_height_is_compact_but_allows_multiline_growth() {
    let mut app = app();

    assert_eq!(app.input_height(40), 3);

    app.input = "one\ntwo\nthree".to_string();
    assert_eq!(app.input_height(40), 5);
}

#[test]
fn input_lines_are_vertically_centered_in_composer() {
    let lines = input_display_lines("", 0, 1);
    let centered = vertically_center_input_lines(lines, 1);

    assert_eq!(centered.len(), 1);
    assert!(
        centered[0]
            .spans
            .iter()
            .any(|span| span.content.contains("Type a task"))
    );
}

#[test]
fn input_display_lines_only_renders_visible_tail_near_cursor() {
    let input = (0..1000)
        .map(|index| format!("line {index}"))
        .collect::<Vec<_>>()
        .join("\n");

    let lines = input_display_lines(&input, input.chars().count(), 3);
    let rendered = lines.iter().map(line_text).collect::<Vec<_>>();

    assert_eq!(lines.len(), 3);
    assert!(rendered[0].contains("line 997"));
    assert!(rendered[1].contains("line 998"));
    assert!(rendered[2].contains("line 999"));
}

#[test]
fn large_paste_inserts_in_one_batch() {
    let mut app = app();
    let paste = (0..1000)
        .map(|index| format!("line {index}"))
        .collect::<Vec<_>>()
        .join("\n");

    app.handle_paste(paste.clone());

    assert_eq!(app.input, paste);
    assert_eq!(app.input_cursor, paste.chars().count());
}

#[test]
fn page_up_and_down_scroll_chat() {
    let mut app = app();

    app.handle_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
    assert_eq!(app.chat_scroll_target, 12);
    assert_eq!(app.chat_scroll, 12);

    app.handle_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
    assert_eq!(app.chat_scroll_target, 0);
    assert_eq!(app.chat_scroll, 0);
}

#[test]
fn page_scroll_uses_chat_viewport_height_when_known() {
    let mut app = scrollback_app(40, Rect::new(0, 0, 80, 10));

    app.handle_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
    assert_eq!(app.chat_scroll_target, 8);
    assert_eq!(app.chat_scroll, 8);

    app.handle_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
    assert_eq!(app.chat_scroll_target, 0);
    assert_eq!(app.chat_scroll, 0);
}

#[test]
fn mouse_wheel_uses_expected_scroll_amounts() {
    let app = scrollback_app(40, Rect::new(0, 0, 80, 10));

    assert_eq!(
        app.mouse_scroll_amount(wheel_event(MouseEventKind::ScrollUp, KeyModifiers::NONE)),
        6
    );
    assert_eq!(
        app.mouse_scroll_amount(wheel_event(MouseEventKind::ScrollUp, KeyModifiers::CONTROL)),
        1
    );
    assert_eq!(
        app.mouse_scroll_amount(wheel_event(MouseEventKind::ScrollUp, KeyModifiers::SHIFT)),
        app.chat_page_scroll_amount()
    );
    assert_eq!(app.chat_page_scroll_amount(), 8);
}

#[test]
fn wheel_events_request_immediate_draw() {
    assert!(event_requests_immediate_draw(&Event::Mouse(wheel_event(
        MouseEventKind::ScrollUp,
        KeyModifiers::NONE
    ))));
    assert!(event_requests_immediate_draw(&Event::Mouse(wheel_event(
        MouseEventKind::ScrollDown,
        KeyModifiers::NONE
    ))));
    assert!(!event_requests_immediate_draw(&Event::Key(KeyEvent::new(
        KeyCode::Char('a'),
        KeyModifiers::NONE
    ))));
}

#[test]
fn ctrl_home_scrolls_to_oldest_visible_content() {
    let mut app = app();
    app.transcript = (0..40)
        .map(|index| TranscriptItem::Message(ChatMessage::assistant(format!("line {index}"))))
        .collect();
    app.last_chat_viewport = Some(Rect::new(0, 0, 80, 10));

    app.handle_key(KeyEvent::new(KeyCode::Home, KeyModifiers::CONTROL));

    assert_eq!(app.chat_scroll, 31);
    assert_eq!(app.chat_scroll_target, 31);
    assert_eq!(app.status_line, "top");
}

#[test]
fn viewport_metrics_anchor_bottom_and_clamp_top() {
    let rows = (0..20)
        .map(|index| TranscriptRow::text(Line::from(format!("line {index}"))))
        .collect::<Vec<_>>();
    let area = Rect::new(0, 0, 80, 5);

    let bottom = chat_viewport_metrics(&rows, area, 0);
    assert_eq!(bottom.max_scroll, 15);
    assert_eq!(bottom.top_offset, 15);

    let middle = chat_viewport_metrics(&rows, area, 10);
    assert_eq!(middle.scroll, 10);
    assert_eq!(middle.top_offset, 5);

    let top = chat_viewport_metrics(&rows, area, usize::MAX);
    assert_eq!(top.scroll, 15);
    assert_eq!(top.top_offset, 0);
}

#[test]
fn user_message_rows_have_background_style() {
    let transcript = vec![TranscriptItem::Message(ChatMessage::user("hello"))];

    let rows = visible_transcript_rows(&transcript, None, None, RenderContext::static_view());
    let user_row = &rows[0].line;

    assert_eq!(line_text(user_row), " › hello ");
    let expected_bg = user_row.spans.first().and_then(|span| span.style.bg);
    assert!(expected_bg.is_some());
    assert!(
        user_row
            .spans
            .iter()
            .all(|span| span.style.bg == expected_bg)
    );
}

#[test]
fn visible_transcript_rows_include_bottom_padding() {
    let transcript = vec![TranscriptItem::Message(ChatMessage::assistant("done"))];

    let rows = visible_transcript_rows(&transcript, None, None, RenderContext::static_view());

    assert_eq!(line_text(&rows.last().unwrap().line), "");
}

#[test]
fn viewport_bottom_anchor_leaves_padding_below_last_message() {
    let rows = (0..5)
        .map(|index| TranscriptRow::text(Line::from(format!("line {index}"))))
        .chain(std::iter::once(TranscriptRow::text(Line::from(""))))
        .collect::<Vec<_>>();
    let area = Rect::new(0, 0, 80, 5);

    let metrics = chat_viewport_metrics(&rows, area, 0);

    assert_eq!(metrics.top_offset, 1);
    assert_eq!(metrics.max_scroll, 1);
}

#[test]
fn viewport_metrics_count_wrapped_visual_lines() {
    let rows = vec![TranscriptRow::text(Line::from("abcdefghijklmnopqrst"))];
    let metrics = chat_viewport_metrics(&rows, Rect::new(0, 0, 10, 1), 0);

    assert_eq!(metrics.total_visual_lines, 2);
    assert_eq!(metrics.max_scroll, 1);
    assert_eq!(metrics.top_offset, 1);
}

#[test]
fn viewport_metrics_keep_text_width_stable_when_overflowing() {
    let rows = (0..20)
        .map(|index| TranscriptRow::text(Line::from(format!("line {index}"))))
        .collect::<Vec<_>>();
    let area = Rect::new(0, 0, 12, 4);

    let metrics = chat_viewport_metrics(&rows, area, 0);

    assert!(metrics.has_scrollbar);
    assert_eq!(metrics.text_area.width, area.width);
}

#[test]
fn scroll_status_reports_position() {
    let mut app = app();
    app.transcript = (0..40)
        .map(|index| TranscriptItem::Message(ChatMessage::assistant(format!("line {index}"))))
        .collect();
    app.last_chat_viewport = Some(Rect::new(0, 0, 80, 10));

    app.scroll_chat_up(8);

    assert!(app.status_line.starts_with("scroll "));
}

#[test]
fn wheel_scroll_updates_visible_offset_immediately() {
    let mut app = scrollback_app(40, Rect::new(0, 0, 80, 10));

    let before = app.current_chat_viewport_metrics().unwrap();
    assert_eq!(before.top_offset, 31);

    app.handle_mouse(wheel_event(MouseEventKind::ScrollUp, KeyModifiers::NONE));

    assert_eq!(app.chat_scroll, 6);
    assert_eq!(app.chat_scroll_target, 6);
    let after = app.current_chat_viewport_metrics().unwrap();
    assert_eq!(after.top_offset, 25);
}

#[test]
fn repeated_wheel_scroll_does_not_build_hidden_scroll_debt() {
    let mut app = scrollback_app(200, Rect::new(0, 0, 80, 10));

    for _ in 0..40 {
        app.handle_mouse(wheel_event(MouseEventKind::ScrollUp, KeyModifiers::NONE));
    }

    assert_eq!(app.chat_scroll, app.chat_scroll_target);
    assert_eq!(
        app.chat_scroll,
        app.current_chat_viewport_metrics().unwrap().max_scroll
    );

    app.handle_mouse(wheel_event(MouseEventKind::ScrollDown, KeyModifiers::NONE));

    assert_eq!(app.chat_scroll, app.chat_scroll_target);
    assert!(app.chat_scroll < app.current_chat_viewport_metrics().unwrap().max_scroll);
}

#[test]
fn viewport_trimmer_skips_each_row_once() {
    let rows = ["a", "b", "c", "d"]
        .into_iter()
        .map(|line| TranscriptRow::text(Line::from(line)))
        .collect::<Vec<_>>();
    let visible = trim_wrapped_lines_for_viewport(&rows, 80, 2, 2);
    let text = visible
        .iter()
        .map(|row| line_text(&row.line))
        .collect::<Vec<_>>();

    assert_eq!(text, vec!["c", "d"]);
}

#[test]
fn ctrl_end_returns_to_bottom() {
    let mut app = app();

    app.chat_scroll = 42;
    app.chat_scroll_target = 42;
    app.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL));

    assert_eq!(app.chat_scroll, 0);
    assert_eq!(app.chat_scroll_target, 0);
}

#[test]
fn workflow_updates_preserve_manual_scrollback() {
    let mut app = app();
    app.transcript = (0..40)
        .map(|index| TranscriptItem::Message(ChatMessage::assistant(format!("line {index}"))))
        .collect();
    app.last_chat_viewport = Some(Rect::new(0, 0, 80, 10));
    app.chat_scroll = 12;
    app.chat_scroll_target = 12;

    app.apply_workflow_event(WorkflowEvent::RunStarted {
        run_id: "run-1".to_string(),
        title: "Build".to_string(),
        task: "task".to_string(),
    });

    assert_eq!(app.chat_scroll, 12);
}

#[test]
fn home_abbreviation_uses_tilde() {
    let home = env::var("HOME").unwrap();
    let nested = format!("{home}/code/project");

    assert_eq!(abbreviate_home(&nested), "~/code/project");
}

#[test]
fn visible_chat_lines_distinguishes_roles() {
    let transcript = vec![
        TranscriptItem::Message(ChatMessage::user("hello")),
        TranscriptItem::Message(ChatMessage::assistant("hi")),
    ];
    let lines = visible_transcript_lines(&transcript, None, None);

    assert_eq!(lines.len(), 3 + CHAT_BOTTOM_PADDING_ROWS);
    let text = lines.iter().map(line_text).collect::<Vec<_>>();
    assert!(text.iter().any(|line| line.contains("hello")));
    assert!(text.iter().any(|line| line.contains("hi")));
}

#[test]
fn reasoning_does_not_render_as_ghost_text() {
    let transcript = vec![
        TranscriptItem::Message(ChatMessage::user("read code")),
        TranscriptItem::Message(ChatMessage::assistant("The render loop is in main.rs.")),
        TranscriptItem::Reasoning(ReasoningTrace {
            content: "Hidden model thinking.".to_string(),
            expanded: false,
        }),
    ];

    let lines = visible_transcript_lines(&transcript, None, None);
    let text = lines.iter().map(line_text).collect::<Vec<_>>();

    assert!(
        text.iter()
            .any(|line| line.contains("The render loop is in main.rs."))
    );
    assert!(
        !text
            .iter()
            .any(|line| line.contains("Hidden model thinking"))
    );
    assert!(!text.iter().any(|line| line.contains("thinking")));
}

#[test]
fn reasoning_is_not_tool_activity_selection() {
    let mut app = app();
    app.transcript
        .push(TranscriptItem::Reasoning(ReasoningTrace {
            content: "Thinking through render order.".to_string(),
            expanded: false,
        }));

    app.select_next_tool();

    assert_eq!(app.selected_tool, None);
    assert_eq!(app.status_line, "no tool activity");
}

#[test]
fn ansi_escape_codes_render_as_colored_spans() {
    let spans = ansi_detail_spans("\u{1b}[31merror\u{1b}[0m: something broke", muted());
    assert!(spans.len() >= 2);
    assert_eq!(spans[0].content.as_ref(), "error");
    assert_eq!(spans[0].style.fg, Some(Color::Red));
    // Unstyled remainder falls back to the muted body style.
    assert_eq!(spans.last().unwrap().style, muted());
}

#[test]
fn rust_code_blocks_get_syntax_highlighting() {
    let lines = markdown_content_lines(
        "```rust\nfn main() { let x = \"hi\"; }\n```",
        ChatRole::Assistant,
    );
    assert_eq!(lines.len(), 1);
    // Border span + several differently-styled token spans.
    assert!(
        lines[0].spans.len() > 3,
        "expected token-level spans, got {:?}",
        lines[0].spans
    );
    let distinct_colors = lines[0]
        .spans
        .iter()
        .skip(1)
        .filter_map(|span| span.style.fg)
        .collect::<std::collections::HashSet<_>>();
    assert!(distinct_colors.len() > 1, "expected multiple token colors");
}

#[test]
fn unknown_language_code_blocks_fall_back_to_plain_style() {
    let lines = markdown_content_lines("```notalanguage\nsome text\n```", ChatRole::Assistant);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].spans.len(), 2);
}

#[test]
fn markdown_content_lines_formats_common_blocks() {
    let content = "# Title\n\n- item `code`\n```rust\nfn main() {}\n```\n> quote";

    let lines = markdown_content_lines(content, ChatRole::Assistant);
    let text = lines.iter().map(line_text).collect::<Vec<_>>();

    assert_eq!(lines.len(), 5);
    assert!(text.iter().any(|line| line.contains("Title")));
    assert!(text.iter().any(|line| line.contains("fn main()")));
    assert!(!text.iter().any(|line| line.contains("code rust")));
}

#[test]
fn clean_model_error_hides_backend_json_payloads() {
    let raw = r#"Codex backend stream ended with response.failed: {"response":{"id":"resp_test","instructions":"You are Medusa, a terminal-native autonomous coding agent","error":{"code":"server_is_overloaded","message":"Our servers are currently overloaded. Please try again later."}}}"#;

    let cleaned = clean_model_error(raw);

    assert_eq!(
        cleaned,
        "model overloaded: Our servers are currently overloaded. Please try again later."
    );
    assert_eq!(model_error_status(&cleaned), "model overloaded");
    assert!(!cleaned.contains("{\"response\""));
    assert!(!cleaned.contains("instructions"));
}

#[test]
fn model_tool_events_update_compact_tool_run() {
    let mut app = app();

    app.push_tool_start("terminal.exec".to_string(), "$ cargo test".to_string());
    for item in &mut app.transcript {
        if let TranscriptItem::Tool(run) = item {
            run.started_at = run
                .started_at
                .checked_sub(MIN_TOOL_PULSE_VISIBLE)
                .unwrap_or(run.started_at);
        }
    }
    app.push_tool_result("terminal.exec", "exit: 0\nstdout:\nok".to_string());

    assert_eq!(app.transcript.len(), 1);
    let TranscriptItem::Tool(inline_run) = &app.transcript[0] else {
        panic!("expected inline tool run");
    };
    assert_eq!(inline_run.name, "terminal.exec");
    assert_eq!(inline_run.state, ToolRunState::Succeeded);
    assert_eq!(inline_run.detail, "ok");
}

#[test]
fn plan_command_toggles_plan_mode() {
    let mut app = app();
    assert!(!app.plan_mode);

    assert!(app.run_local_tool_command("/plan"));
    assert!(app.plan_mode);
    assert!(app.status_line.contains("plan mode on"));
    let history = app.conversation_history();
    assert!(
        history
            .iter()
            .any(|message| message.role == "system"
                && message.content.contains("Plan mode is active"))
    );

    assert!(app.run_local_tool_command("/plan"));
    assert!(!app.plan_mode);
    assert!(
        !app.conversation_history()
            .iter()
            .any(|message| message.content.contains("Plan mode is active"))
    );
}

#[test]
fn shift_tab_toggles_plan_mode_in_composer() {
    let mut app = app();

    app.handle_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
    assert!(app.plan_mode);

    app.handle_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
    assert!(!app.plan_mode);
}

#[test]
fn single_escape_never_quits_idle_composer() {
    let mut app = app();

    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

    assert!(!app.should_quit);
    assert_eq!(app.status_line, "press esc again to quit");
}

#[test]
fn double_escape_quits_within_window() {
    let mut app = app();

    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

    assert!(app.should_quit);
}

#[test]
fn stale_escape_does_not_count_toward_quit() {
    let mut app = app();

    app.last_escape_at = Instant::now().checked_sub(DOUBLE_ESCAPE_WINDOW * 2);
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

    assert!(!app.should_quit);
    assert_eq!(app.status_line, "press esc again to quit");
}

#[test]
fn escape_clears_input_before_arming_quit() {
    let mut app = app();
    app.input = "half-typed task".to_string();
    app.input_cursor = app.input_len();

    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(!app.should_quit);
    assert!(app.input.is_empty());
    assert_eq!(app.status_line, "input cleared");

    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(!app.should_quit);
}

#[test]
fn escape_exits_plan_mode_before_arming_quit() {
    let mut app = app();
    app.plan_mode = true;

    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(!app.plan_mode);
    assert!(!app.should_quit);
    assert!(app.status_line.contains("plan mode off"));

    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(!app.should_quit);
    assert_eq!(app.status_line, "press esc again to quit");
}

#[test]
fn escape_deselects_tool_before_arming_quit() {
    let mut app = app();
    app.transcript.push(TranscriptItem::Tool(ToolRun {
        id: None,
        started_at: Instant::now(),
        pending_result: None,
        name: "terminal.exec".to_string(),
        summary: "$ ls".to_string(),
        state: ToolRunState::Succeeded,
        detail: "done".to_string(),
        expanded: true,
        group_expanded: false,
    }));
    app.selected_tool = Some(0);

    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

    assert!(!app.should_quit);
    assert_eq!(app.selected_tool, None);
}
