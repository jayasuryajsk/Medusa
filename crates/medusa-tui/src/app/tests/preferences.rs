use super::*;

#[test]
fn theme_preference_round_trips_through_workspace_settings() {
    let workspace = temp_workspace();

    save_theme_preference(&workspace, ThemeKind::Gruvbox).unwrap();

    let settings = load_app_settings(&workspace).unwrap();
    assert_eq!(settings.theme(), Some(ThemeKind::Gruvbox));
    assert_eq!(
        ThemeKind::from_workspace_settings(&workspace),
        ThemeKind::Gruvbox
    );
}

#[test]
fn env_theme_overrides_workspace_settings() {
    let workspace = temp_workspace();
    save_theme_preference(&workspace, ThemeKind::Gruvbox).unwrap();

    // Pass the override explicitly instead of mutating process-global env:
    // `set_var("MEDUSA_THEME", …)` races the parallel test harness's
    // getenv-backed readers (UB) and its sibling `from_workspace_settings`
    // callers (flaky logic race).
    assert_eq!(
        ThemeKind::resolve(Some("nord"), &workspace),
        ThemeKind::Nord
    );
    // With no override the persisted workspace setting wins.
    assert_eq!(ThemeKind::resolve(None, &workspace), ThemeKind::Gruvbox);
}

#[test]
fn typing_updates_input() {
    let mut app = app();

    app.handle_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE));

    assert_eq!(app.input, "hi");
    assert_eq!(app.input_cursor, 2);
}

#[test]
fn backspace_edits_input() {
    let mut app = app();

    app.input = "fixx".to_string();
    app.input_cursor = 4;
    app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));

    assert_eq!(app.input, "fix");
    assert_eq!(app.input_cursor, 3);
}

#[test]
fn cursor_allows_mid_line_edits() {
    let mut app = app();

    app.input = "helo".to_string();
    app.input_cursor = 2;
    app.handle_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE));

    assert_eq!(app.input, "hell");
    assert_eq!(app.input_cursor, 4);
}

#[test]
fn shift_enter_inserts_newline() {
    let mut app = app();

    app.input = "one".to_string();
    app.input_cursor = 3;
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
    app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE));

    assert_eq!(app.input, "one\nt");
    assert_eq!(app.input_cursor, 5);
}

#[test]
fn alt_enter_also_inserts_newline() {
    let mut app = app();

    app.input = "one".to_string();
    app.input_cursor = 3;
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT));

    assert_eq!(app.input, "one\n");
    assert_eq!(app.input_cursor, 4);
}

#[test]
fn up_and_down_move_between_input_lines_keeping_column() {
    let mut app = app();

    app.input = "first line\nsecond\nthird line".to_string();
    // Cursor at column 8 of the last line ("third li|ne").
    app.input_cursor = "first line\nsecond\nthird li".chars().count();

    app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    // "second" has 6 chars; column clamps to its end.
    assert_eq!(app.input_cursor, "first line\nsecond".chars().count());

    app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(app.input_cursor, 6, "column carries to the longer line");

    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(
        app.input_cursor,
        "first line\nsecond\nthird ".chars().count()
    );
}

#[test]
fn home_and_end_are_line_local_in_multiline_input() {
    let mut app = app();

    app.input = "first\nsecond".to_string();
    app.input_cursor = "first\nsec".chars().count();

    app.handle_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
    assert_eq!(app.input_cursor, "first\n".chars().count());

    app.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
    assert_eq!(app.input_cursor, "first\nsecond".chars().count());
}

#[test]
fn composer_attachment_preview_has_fixed_height_and_overflow() {
    let attachments = vec![
        image_attachment("one"),
        image_attachment("two"),
        image_attachment("three"),
    ];
    let previews = attachments
        .iter()
        .map(|attachment| {
            (
                attachment.id.clone(),
                image_preview_lines(attachment, COMPOSER_IMAGE_PREVIEW_WIDTH),
            )
        })
        .collect::<HashMap<_, _>>();

    let lines = composer_attachment_preview_lines(&attachments, &previews, 42);
    let text = lines.iter().map(line_text).collect::<Vec<_>>();

    assert_eq!(lines.len(), COMPOSER_IMAGE_PREVIEW_HEIGHT as usize);
    assert!(text[0].contains("+1"));
    assert!(text.iter().any(|line| line.contains("320×180")));
}

#[test]
fn transcript_rows_reserve_real_image_area_for_attachments() {
    let attachment = image_attachment("screenshot");
    let transcript = vec![TranscriptItem::Message(ChatMessage::user_with_attachments(
        "look at this",
        vec![attachment.clone()],
    ))];

    let rows = visible_transcript_rows(&transcript, None, None, RenderContext::static_view());
    let image_rows = rows.iter().filter(|row| row.image.is_some()).count();
    let text = rows
        .iter()
        .map(|row| line_text(&row.line))
        .collect::<Vec<_>>();

    assert_eq!(image_rows, 1);
    assert!(
        rows.iter()
            .any(|row| row.image.as_ref() == Some(&attachment))
    );
    assert!(text.iter().any(|line| line.contains("look at this")));
    assert!(text.iter().any(|line| line.contains("screenshot.png")));
    assert!(rows.len() >= CHAT_IMAGE_PREVIEW_HEIGHT as usize);
}

#[test]
fn transcript_image_placement_survives_partial_scroll() {
    let attachment = image_attachment("screenshot");
    let mut rows = vec![
        TranscriptRow::text(Line::from("before image")),
        TranscriptRow::image(Line::from("image placeholder"), attachment.clone()),
    ];
    rows.extend((1..CHAT_IMAGE_PREVIEW_HEIGHT).map(|_| TranscriptRow::text(Line::from(""))));
    let area = Rect::new(0, 0, 80, 6);

    let placements = transcript_image_placements(&rows, area, 3);

    assert_eq!(placements.len(), 1);
    assert_eq!(placements[0].attachment, attachment);
    assert_eq!(placements[0].width, CHAT_IMAGE_PREVIEW_WIDTH);
    assert_eq!(placements[0].height, CHAT_IMAGE_PREVIEW_HEIGHT);
    assert_eq!(placements[0].x_offset, 2);
    assert_eq!(placements[0].y_offset, -2);
}

#[test]
fn transcript_image_placement_skips_images_above_viewport() {
    let attachment = image_attachment("screenshot");
    let mut rows = vec![
        TranscriptRow::text(Line::from("before image")),
        TranscriptRow::image(Line::from("image placeholder"), attachment),
    ];
    rows.extend((1..CHAT_IMAGE_PREVIEW_HEIGHT).map(|_| TranscriptRow::text(Line::from(""))));
    let area = Rect::new(0, 0, 80, 6);

    let placements =
        transcript_image_placements(&rows, area, CHAT_IMAGE_PREVIEW_HEIGHT as usize + 2);

    assert!(placements.is_empty());
}

#[test]
fn images_command_is_gone_but_ctrl_o_still_previews() {
    let mut app = app();
    app.pending_attachments.push(image_attachment("clipboard"));

    assert!(app.run_local_tool_command("/images"));
    assert_ne!(app.active_modal, Some(Modal::ImagePreview));

    app.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
    assert_eq!(app.active_modal, Some(Modal::ImagePreview));
}

#[test]
fn ctrl_o_opens_latest_image_preview() {
    let mut app = app();
    app.transcript
        .push(TranscriptItem::Message(ChatMessage::user_with_attachments(
            "look",
            vec![image_attachment("one"), image_attachment("two")],
        )));

    app.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));

    assert_eq!(app.active_modal, Some(Modal::ImagePreview));
    assert_eq!(app.image_preview_index, 1);
}

#[test]
fn image_preview_navigation_and_zoom_are_bounded() {
    let mut app = app();
    app.pending_attachments.push(image_attachment("one"));
    app.pending_attachments.push(image_attachment("two"));
    app.open_image_preview(0);

    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
    assert_eq!(app.image_preview_index, 1);
    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
    assert_eq!(app.image_preview_index, 0);

    for _ in 0..20 {
        app.handle_key(KeyEvent::new(KeyCode::Char('+'), KeyModifiers::NONE));
    }
    assert_eq!(app.image_preview_zoom, IMAGE_PREVIEW_MAX_ZOOM);
    for _ in 0..20 {
        app.handle_key(KeyEvent::new(KeyCode::Char('-'), KeyModifiers::NONE));
    }
    assert_eq!(app.image_preview_zoom, IMAGE_PREVIEW_MIN_ZOOM);
    app.handle_key(KeyEvent::new(KeyCode::Char('0'), KeyModifiers::NONE));
    assert_eq!(app.image_preview_zoom, 100);
}

#[test]
fn ctrl_d_detaches_latest_pending_attachment() {
    let mut app = app();
    let first = image_attachment("one");
    let second = image_attachment("two");
    app.cache_attachment_preview(&first);
    app.cache_attachment_preview(&second);
    app.pending_attachments.push(first.clone());
    app.pending_attachments.push(second.clone());

    app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));

    assert_eq!(app.pending_attachments, vec![first]);
    assert!(!app.attachment_previews.contains_key(&second.id));
    assert_eq!(app.status_line, "detached latest image");
}

#[test]
fn preview_delete_detaches_pending_image_and_keeps_sent_images() {
    let mut app = app();
    let sent = image_attachment("sent");
    let pending = image_attachment("pending");
    app.transcript
        .push(TranscriptItem::Message(ChatMessage::user_with_attachments(
            "sent",
            vec![sent.clone()],
        )));
    app.pending_attachments.push(pending);
    app.open_latest_image_preview();

    app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));

    assert!(app.pending_attachments.is_empty());
    assert_eq!(app.active_modal, Some(Modal::ImagePreview));
    assert_eq!(app.current_preview_image(), Some(sent));
    assert_eq!(app.image_preview_index, 0);
}

#[test]
fn preview_delete_refuses_sent_image() {
    let mut app = app();
    let sent = image_attachment("sent");
    app.transcript
        .push(TranscriptItem::Message(ChatMessage::user_with_attachments(
            "sent",
            vec![sent.clone()],
        )));
    app.open_image_preview(0);

    app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));

    assert_eq!(app.current_preview_image(), Some(sent));
    assert_eq!(app.active_modal, Some(Modal::ImagePreview));
    assert_eq!(app.status_line, "sent image stays in transcript");
}

#[test]
fn image_input_warning_only_shows_for_chat_backends() {
    assert_eq!(image_input_warning("codex"), None);
    assert!(image_input_warning("deepseek").is_some());
    assert!(image_input_warning("openai-compatible").is_some());
}

#[test]
fn clicking_transcript_image_opens_preview() {
    let mut app = app();
    let attachment = image_attachment("screenshot");
    app.pending_attachments.push(attachment.clone());
    app.last_chat_viewport = Some(Rect::new(0, 0, 80, 20));
    let mut rows = vec![
        TranscriptRow::text(Line::from("before image")),
        TranscriptRow::image(Line::from("image placeholder"), attachment),
    ];
    rows.extend((1..CHAT_IMAGE_PREVIEW_HEIGHT).map(|_| TranscriptRow::text(Line::from(""))));
    app.last_transcript_rows = Arc::new(rows);

    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 4,
        row: 2,
        modifiers: KeyModifiers::NONE,
    });

    assert_eq!(app.active_modal, Some(Modal::ImagePreview));
    assert_eq!(app.image_preview_index, 0);
}

#[test]
fn preview_image_dimensions_fit_at_default_zoom_and_scale_up() {
    let attachment = image_attachment("wide");
    let area = Rect::new(0, 0, 80, 20);

    let fit = preview_image_dimensions(&attachment, area, 100);
    let zoomed = preview_image_dimensions(&attachment, area, 200);

    assert!(fit.0 <= area.width);
    assert!(fit.1 <= area.height);
    assert!(zoomed.0 >= fit.0);
    assert!(zoomed.1 >= fit.1);
}

#[test]
fn enter_captures_task() {
    let mut app = app();

    app.input = "fix tests".to_string();
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    assert_eq!(app.input, "");
    assert_eq!(
        app.transcript,
        vec![TranscriptItem::Message(ChatMessage::user("fix tests"))]
    );
}

#[test]
fn control_j_submits_task_for_pty_enter() {
    let mut app = app();

    app.input = "fix tests".to_string();
    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));

    assert_eq!(app.input, "");
    assert_eq!(
        app.transcript,
        vec![TranscriptItem::Message(ChatMessage::user("fix tests"))]
    );
}

#[test]
fn help_command_lists_slash_commands() {
    let mut app = app();

    app.input = "/help".to_string();
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    assert_eq!(app.status_line, "help opened");
    assert_eq!(app.active_modal, Some(Modal::Help));
}

#[test]
fn settings_command_opens_settings_modal() {
    let mut app = app();
    let expected_theme = app.theme.name().to_string();
    let expected_reasoning = app.model.reasoning_effort().to_string();

    app.input = "/settings".to_string();
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    assert_eq!(app.status_line, "settings opened");
    assert_eq!(app.active_modal, Some(Modal::Settings));
    assert!(
        app.settings_rows()
            .iter()
            .any(|(key, value)| { *key == "model" && value == "gpt-5.5" })
    );
    assert!(
        app.settings_rows()
            .iter()
            .any(|(key, value)| { *key == "reasoning" && value == &expected_reasoning })
    );
    assert!(
        app.settings_rows()
            .iter()
            .any(|(key, value)| { *key == "theme" && value == &expected_theme })
    );
    assert!(
        app.settings_rows()
            .iter()
            .any(|(key, value)| { *key == "permissions" && value == "guarded" })
    );
}

#[test]
fn model_command_switches_model_and_persists_setting() {
    let (mut app, workspace) = app_in_workspace();

    app.input = "/model gpt-test-model".to_string();
    app.input_cursor = app.input_len();
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    assert_eq!(app.model.model_name(), "gpt-test-model");
    assert_eq!(app.status_line, "model: gpt-test-model");
    let settings = load_app_settings(&workspace).unwrap();
    assert_eq!(settings.model(), Some("gpt-test-model".to_string()));
}

#[test]
fn selecting_model_from_palette_opens_model_picker() {
    let mut app = app();

    app.input = "/model".to_string();
    app.input_cursor = app.input_len();
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    assert_eq!(app.active_modal, Some(Modal::Models));
    assert_eq!(app.input, "");
    assert_eq!(app.model_picker_pane, ModelPickerPane::Models);
    assert_eq!(app.status_line, "model and execution mode picker opened");
}

#[test]
fn model_picker_steps_into_reasoning_and_persists_both() {
    let (mut app, workspace) = app_in_workspace();
    app.model.set_model_name("custom-test-model");
    app.model.set_reasoning_effort("medium");

    app.open_models_modal();
    assert_eq!(app.model_picker_pane, ModelPickerPane::Models);
    assert_eq!(
        app.reasoning_selection,
        reasoning_index("custom-test-model", "medium")
    );

    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(app.active_modal, Some(Modal::Models));
    assert_eq!(app.model_picker_pane, ModelPickerPane::Reasoning);

    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    assert_eq!(app.active_modal, None);
    assert_eq!(app.model.model_name(), "custom-test-model");
    assert_eq!(app.model.reasoning_effort(), "high");
    assert_eq!(
        app.status_line,
        "model: custom-test-model · reasoning: high"
    );
    let settings = load_app_settings(&workspace).unwrap();
    assert_eq!(settings.model(), Some("custom-test-model".to_string()));
    assert_eq!(settings.reasoning_effort(), Some("high".to_string()));
}

#[test]
fn model_picker_renders_model_mode_and_selection_panes() {
    use ratatui::{Terminal, backend::TestBackend};

    let mut app = app();
    app.open_models_modal();
    let mut terminal = Terminal::new(TestBackend::new(112, 24)).unwrap();
    terminal
        .draw(|frame| {
            let area = frame.area();
            app.draw_modal(frame, area);
        })
        .unwrap();
    let rendered = buffer_text(terminal.backend().buffer());

    assert!(rendered.contains("Model & execution mode"), "{rendered}");
    assert!(rendered.contains("MODEL"), "{rendered}");
    assert!(rendered.contains("EFFORT / MODE"), "{rendered}");
    assert!(rendered.contains("SELECTION"), "{rendered}");
}

#[test]
fn header_shows_current_model_and_reasoning_effort() {
    use ratatui::{Terminal, backend::TestBackend};

    let (mut app, _workspace) = app_in_workspace();
    app.model.set_model_name("gpt-5.6-sol");
    app.model.set_reasoning_effort("high");

    let mut terminal = Terminal::new(TestBackend::new(120, 2)).unwrap();
    terminal
        .draw(|frame| app.draw_header(frame, frame.area()))
        .unwrap();
    let rendered = buffer_text(terminal.backend().buffer());

    assert!(
        rendered.contains("gpt-5.6-sol"),
        "header should show the model: {rendered:?}"
    );
    assert!(
        rendered.contains("high"),
        "header should show the reasoning effort: {rendered:?}"
    );
}

fn buffer_text(buffer: &ratatui::buffer::Buffer) -> String {
    let area = buffer.area();
    let mut out = String::new();
    for y in 0..area.height {
        for x in 0..area.width {
            out.push_str(buffer[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

#[test]
fn reasoning_command_sets_effort_and_persists_setting() {
    let (mut app, workspace) = app_in_workspace();

    app.input = "/reasoning high".to_string();
    app.input_cursor = app.input_len();
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    assert_eq!(app.model.reasoning_effort(), "high");
    assert_eq!(app.status_line, "reasoning: high");
    let settings = load_app_settings(&workspace).unwrap();
    assert_eq!(settings.reasoning_effort(), Some("high".to_string()));
}

#[test]
fn reasoning_command_opens_picker_and_enter_saves_selection() {
    let (mut app, _workspace) = app_in_workspace();

    app.input = "/reasoning".to_string();
    app.input_cursor = app.input_len();
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(app.active_modal, Some(Modal::Reasoning));

    // The picker is seeded to the active effort; moving + Enter applies a
    // different one and closes.
    let before = app.model.reasoning_effort().to_string();
    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(app.active_modal, None);
    assert_ne!(app.model.reasoning_effort(), before);
}

#[test]
fn reasoning_effort_preference_round_trips_through_settings() {
    let workspace = temp_workspace();
    save_reasoning_preference(&workspace, "xhigh").unwrap();
    let settings = load_app_settings(&workspace).unwrap();
    assert_eq!(settings.reasoning_effort(), Some("xhigh".to_string()));
    // Blank/whitespace is normalized away so the backend default applies.
    save_reasoning_preference(&workspace, "   ").unwrap();
    assert_eq!(
        load_app_settings(&workspace).unwrap().reasoning_effort(),
        None
    );
}

#[test]
fn fresh_workspaces_default_to_guarded_permissions() {
    let workspace = temp_workspace();
    let settings = load_app_settings(&workspace).unwrap();

    assert_eq!(settings.permission_mode(), PermissionMode::Guarded);
}

#[test]
fn permission_command_switches_mode_and_updates_runtime() {
    let (mut app, workspace) = app_in_workspace();

    app.input = "/permissions readonly".to_string();
    app.input_cursor = app.input_len();
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    assert_eq!(app.permission_mode, PermissionMode::Readonly);
    assert_eq!(app.status_line, "permissions: readonly");
    let settings = load_app_settings(&workspace).unwrap();
    assert_eq!(settings.permission_mode(), PermissionMode::Readonly);
    let denied = app
        .tools
        .file_patch(FilePatchRequest::new(
            "*** Begin Patch\n*** Add File: writable.txt\n+nope\n*** End Patch\n",
        ))
        .unwrap_err()
        .to_string();
    assert!(denied.contains("does not match an allow_prefixes entry"));
}

#[test]
fn readonly_mode_is_visible_in_header_and_status() {
    let mut app = app();
    app.permission_mode = PermissionMode::Readonly;

    let (label, _) = app.header_state();

    assert_eq!(label, "readonly");
    assert_eq!(app.scoped_status("streaming"), "readonly · streaming");
}

#[test]
fn conversation_history_includes_permission_context() {
    let mut app = app();
    app.permission_mode = PermissionMode::Readonly;
    app.transcript
        .push(TranscriptItem::Message(ChatMessage::user("read codebase")));

    let messages = app.conversation_history();

    assert_eq!(
        messages.first().map(|message| message.role.as_str()),
        Some("system")
    );
    assert!(
        messages
            .first()
            .is_some_and(|message| message.content.contains("permission mode: readonly"))
    );
    assert!(messages.first().is_some_and(|message| {
        message
            .content
            .contains("File mutation tools are unavailable")
    }));
    assert!(
        messages
            .iter()
            .any(|message| message.content == "read codebase")
    );
}

#[test]
fn conversation_history_includes_rolling_session_state() {
    let mut app = app();
    for index in 0..40 {
        app.transcript
            .push(TranscriptItem::Message(ChatMessage::user(format!(
                "old task {index}"
            ))));
        app.transcript
            .push(TranscriptItem::Message(ChatMessage::assistant(format!(
                "old outcome {index}"
            ))));
    }

    let messages = app.conversation_history();

    assert_eq!(
        messages.get(1).map(|message| message.role.as_str()),
        Some("system")
    );
    assert!(messages[1].content.contains("Medusa rolling session state"));
    assert!(messages[1].content.contains("old task 39"));
    // Full history flows through; the ContextEngine compacts at turn
    // start only when the token budget requires it.
    assert!(
        messages
            .iter()
            .any(|message| message.role == "user" && message.content == "old task 0")
    );
    assert!(
        messages
            .iter()
            .any(|message| message.role == "user" && message.content == "old task 39")
    );
}

#[test]
fn session_state_preserves_semantic_memory_outside_recent_window() {
    let mut app = app();
    app.transcript
        .push(TranscriptItem::Message(ChatMessage::user(
            "I prefer concise answers and do not touch auth code.",
        )));
    for index in 0..40 {
        app.transcript
            .push(TranscriptItem::Message(ChatMessage::user(format!(
                "noise task {index}"
            ))));
        app.transcript
            .push(TranscriptItem::Message(ChatMessage::assistant(format!(
                "noise outcome {index}"
            ))));
    }

    let messages = app.conversation_history();

    assert!(messages[1].content.contains("semantic memory"));
    assert!(
        messages[1]
            .content
            .contains("preference: I prefer concise answers")
    );
}

#[test]
fn session_state_summarizes_tool_file_mentions() {
    let mut app = app();
    app.transcript.push(TranscriptItem::Tool(ToolRun {
        id: None,
        started_at: Instant::now(),
        pending_result: None,
        name: "file.patch".to_string(),
        summary: "edited crates/medusa-tui/src/main.rs".to_string(),
        state: ToolRunState::Succeeded,
        detail: "also touched README.md".to_string(),
        expanded: false,
        group_expanded: false,
    }));

    let messages = app.conversation_history();

    assert!(messages[1].content.contains("tool history"));
    assert!(messages[1].content.contains("file.patch succeeded"));
    assert!(messages[1].content.contains("changed or referenced files"));
    assert!(
        messages[1]
            .content
            .contains("crates/medusa-tui/src/main.rs")
    );
    assert!(messages[1].content.contains("README.md"));
}

#[test]
fn settings_modal_can_open_model_and_permission_pickers() {
    let mut app = app();

    app.open_settings_modal();
    app.settings_selection = app
        .settings_items()
        .iter()
        .position(|item| item.key == "model")
        .unwrap();
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(app.active_modal, Some(Modal::Models));

    app.open_settings_modal();
    app.settings_selection = app
        .settings_items()
        .iter()
        .position(|item| item.key == "reasoning")
        .unwrap();
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(app.active_modal, Some(Modal::Reasoning));

    app.open_settings_modal();
    app.settings_selection = app
        .settings_items()
        .iter()
        .position(|item| item.key == "permissions")
        .unwrap();
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(app.active_modal, Some(Modal::Permissions));
}

#[test]
fn slash_prefix_suggests_settings() {
    let mut app = app();

    app.input = "/se".to_string();
    app.input_cursor = 3;

    let matches = app.slash_matches();
    assert!(
        matches
            .iter()
            .any(|(command, _)| command.name == "/settings")
    );
}

#[test]
fn fuzzy_subsequence_matches_commands() {
    let mut app = app();

    app.input = "/wf".to_string();
    app.input_cursor = 3;

    let matches = app.slash_matches();
    let workflow = matches
        .iter()
        .find(|(command, _)| command.name == "/workflow")
        .expect("fuzzy match for /workflow");
    assert_eq!(workflow.1, vec![0, 4]);
}

#[test]
fn enter_on_fully_typed_command_runs_it_directly() {
    let mut app = app();

    app.input = "/help".to_string();
    app.input_cursor = app.input_len();
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    assert_eq!(app.active_modal, Some(Modal::Help));
    assert!(app.input.is_empty());
}

#[test]
fn slash_prefix_suggests_fork() {
    let mut app = app();

    app.input = "/fo".to_string();
    app.input_cursor = 3;

    let matches = app.slash_matches();
    assert!(matches.iter().any(|(command, _)| command.name == "/fork"));
}

#[test]
fn slash_prefix_suggests_rewind() {
    let mut app = app();

    app.input = "/re".to_string();
    app.input_cursor = 3;

    let matches = app.slash_matches();
    assert!(matches.iter().any(|(command, _)| command.name == "/rewind"));
}
