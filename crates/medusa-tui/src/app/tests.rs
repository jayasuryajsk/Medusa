use super::*;

use std::sync::atomic::Ordering;

use ratatui::style::Color;

use crate::cli::{
    HELP_TEXT, HeadlessOptions, RUN_HELP_TEXT, StartupCommand, VERSION_TEXT, parse_startup_command,
};
use crate::markdown::markdown_content_lines;

use medusa_core::session::{compact_session_id, normalize_session_name, read_session_file};
use medusa_core::workflow::SubagentToolPolicy;

fn app() -> App {
    App::with_model_backend(false)
}

/// Wrap a raw workflow-event receiver into a `BackgroundWorkflow` for tests
/// that push directly onto `app.workflow_events`.
fn background_workflow(app: &App, events: Receiver<WorkflowEvent>) -> BackgroundWorkflow {
    BackgroundWorkflow {
        events,
        checkpoint: app.new_workflow_checkpoint("/workflow test", 0),
        cancel: CancelToken::new(),
    }
}

fn write_saved_workflow(app: &App, name: &str, source: &str) {
    let directory = app.tools.workspace().join(".medusa/workflows");
    fs::create_dir_all(&directory).unwrap();
    fs::write(directory.join(format!("{name}.js")), source).unwrap();
}

fn line_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

fn first_span_fg_containing(rows: &[TranscriptRow], needle: &str) -> Option<Color> {
    rows.iter()
        .flat_map(|row| row.line.spans.iter())
        .find(|span| span.content.contains(needle))
        .and_then(|span| span.style.fg)
}

fn wheel_event(kind: MouseEventKind, modifiers: KeyModifiers) -> MouseEvent {
    MouseEvent {
        kind,
        column: 0,
        row: 0,
        modifiers,
    }
}

fn scrollback_app(line_count: usize, viewport: Rect) -> App {
    let mut app = app();
    app.transcript = (0..line_count)
        .map(|index| TranscriptItem::Message(ChatMessage::assistant(format!("line {index}"))))
        .collect();
    app.last_chat_viewport = Some(viewport);
    app
}

fn image_attachment(id: &str) -> ImageAttachment {
    ImageAttachment {
        id: id.to_string(),
        name: format!("{id}.png"),
        path: PathBuf::from(format!("/tmp/{id}.png")),
        mime: "image/png".to_string(),
        width: 320,
        height: 180,
        size_bytes: 42_000,
    }
}

fn temp_workspace() -> PathBuf {
    // pid + atomic counter: unique across parallel test threads and
    // concurrent test processes (a bare timestamp raced in the past).
    static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = env::temp_dir().join(format!("medusa-tui-test-{}-{suffix}", std::process::id()));
    fs::create_dir_all(&path).unwrap();
    path.canonicalize().unwrap()
}

fn app_in_workspace() -> (App, PathBuf) {
    let workspace = temp_workspace();
    let mut app = app();
    app.tools = ToolRuntime::new(&workspace).unwrap();
    app.model = DirectCodexBackend::new(&workspace).unwrap();
    app.cwd_display = abbreviate_home(&workspace.to_string_lossy());
    (app, workspace)
}

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

fn checkpoint_in(workspace: &Path, prompt: &str, user_index: usize) -> String {
    let recorder = CheckpointRecorder::new(
        workspace,
        CheckpointMeta {
            session_id: "session-elsewhere.json".to_string(),
            prompt_excerpt: prompt.to_string(),
            transcript_user_index: user_index,
        },
    );
    fs::write(workspace.join("tracked.txt"), format!("{prompt}\n")).unwrap();
    recorder.capture(&["tracked.txt".to_string()]).unwrap();
    fs::write(workspace.join("tracked.txt"), "mutated\n").unwrap();
    recorder.finish().unwrap().id
}

#[test]
fn rewind_opens_modal_with_entries_newest_first() {
    let mut app = app();
    let workspace = app.tools.workspace().to_path_buf();
    let first = checkpoint_in(&workspace, "first turn", 0);
    let second = checkpoint_in(&workspace, "second turn", 2);

    assert!(app.run_local_tool_command("/rewind"));

    assert_eq!(app.active_modal, Some(Modal::Rewind));
    assert_eq!(app.rewind_stage, RewindStage::Pick);
    assert_eq!(app.rewind_entries.len(), 2);
    assert_eq!(app.rewind_entries[0].id, second);
    assert_eq!(app.rewind_entries[1].id, first);
    assert_eq!(app.rewind_entries[0].prompt_excerpt, "second turn");
}

#[test]
fn rewind_without_checkpoints_stays_closed() {
    let mut app = app();

    assert!(app.run_local_tool_command("/rewind"));

    assert_eq!(app.active_modal, None);
    assert_eq!(app.status_line, "no checkpoints yet");
}

#[test]
fn rewind_is_refused_while_a_turn_is_streaming() {
    let mut app = app();
    let workspace = app.tools.workspace().to_path_buf();
    checkpoint_in(&workspace, "some turn", 0);
    let (_sender, receiver) = mpsc::channel::<ModelStreamEvent>();
    app.model_events = Some(receiver);

    assert!(app.run_local_tool_command("/rewind"));

    assert_eq!(app.active_modal, None);
    assert_eq!(app.status_line, "finish the current turn before rewinding");
}

#[test]
fn rewind_restore_rewinds_files_and_closes_modal() {
    let mut app = app();
    let workspace = app.tools.workspace().to_path_buf();
    let id = checkpoint_in(&workspace, "edit tracked", 0);
    assert_eq!(
        fs::read_to_string(workspace.join("tracked.txt")).unwrap(),
        "mutated\n"
    );

    assert!(app.run_local_tool_command("/rewind"));
    assert_eq!(app.rewind_entries[0].id, id);
    // Pick the checkpoint, then confirm the default "Restore files".
    app.handle_rewind_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(app.rewind_stage, RewindStage::Confirm);
    // Foreign-session checkpoint: no fork option offered.
    assert_eq!(
        app.rewind_confirm_options(),
        vec!["Restore files", "Cancel"]
    );
    app.handle_rewind_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    assert_eq!(app.active_modal, None);
    assert_eq!(
        fs::read_to_string(workspace.join("tracked.txt")).unwrap(),
        "edit tracked\n"
    );
}

#[test]
fn rewind_fork_truncates_transcript_and_prefills_composer() {
    let mut app = app();
    let workspace = app.tools.workspace().to_path_buf();
    let session = SessionStore::open(&workspace, SessionOpenMode::New).unwrap();
    let original_id = session.current_id();
    session.save_transcript(&[]).unwrap();
    app.session = Some(session);
    app.transcript = vec![
        TranscriptItem::Message(ChatMessage::user("first prompt")),
        TranscriptItem::Message(ChatMessage::assistant("first answer")),
        TranscriptItem::Message(ChatMessage::user("second prompt")),
        TranscriptItem::Message(ChatMessage::assistant("second answer")),
    ];
    let entry = CheckpointEntry {
        id: "cp-test".to_string(),
        session_id: original_id.clone(),
        created_at_ms: 0,
        prompt_excerpt: "second prompt".to_string(),
        transcript_user_index: 2,
        parent_id: None,
        note: String::new(),
        files: Vec::new(),
    };

    app.fork_transcript_at_checkpoint(&entry);

    assert_eq!(app.transcript.len(), 2);
    assert!(matches!(
        &app.transcript[1],
        TranscriptItem::Message(message) if message.content == "first answer"
    ));
    assert_eq!(app.input, "second prompt");
    let forked_id = app.session.as_ref().unwrap().current_id();
    assert_ne!(forked_id, original_id);
    assert_eq!(
        app.session.as_ref().unwrap().parent_id(),
        Some(original_id.as_str())
    );
}

/// Finding [20]: `/clear` empties the transcript without rotating the
/// session id, so a pre-clear checkpoint's `transcript_user_index` is now
/// stale. Session-id equality alone must NOT offer fork.
#[test]
fn rewind_hides_fork_for_stale_index_after_clear() {
    let mut app = app();
    let workspace = app.tools.workspace().to_path_buf();
    let session = SessionStore::open(&workspace, SessionOpenMode::New).unwrap();
    let session_id = session.current_id();
    app.session = Some(session);
    // Same session id, index 8 recorded before /clear.
    app.rewind_entries = vec![CheckpointEntry {
        id: "cp-stale".to_string(),
        session_id: session_id.clone(),
        created_at_ms: 1,
        prompt_excerpt: "old prompt".to_string(),
        transcript_user_index: 8,
        parent_id: None,
        note: String::new(),
        files: Vec::new(),
    }];
    app.rewind_selection = 0;
    // /clear then one fresh turn: the transcript no longer has row 8.
    app.transcript = vec![
        TranscriptItem::Message(ChatMessage::user("brand new prompt")),
        TranscriptItem::Message(ChatMessage::assistant("answer")),
    ];

    assert!(
        !app.selected_rewind_offers_fork(),
        "stale index must not offer fork even though session id still matches"
    );
    assert_eq!(
        app.rewind_confirm_options(),
        vec!["Restore files", "Cancel"]
    );
}

/// Finding [20]: even if the confirm option is reached directly, forking on
/// a stale index must not truncate the live transcript, overwrite the
/// composer, rotate the session, or toast a false success.
#[test]
fn fork_at_checkpoint_refuses_stale_index_and_leaves_conversation_intact() {
    let mut app = app();
    let workspace = app.tools.workspace().to_path_buf();
    let session = SessionStore::open(&workspace, SessionOpenMode::New).unwrap();
    let original_id = session.current_id();
    session.save_transcript(&[]).unwrap();
    app.session = Some(session);
    app.transcript = vec![
        TranscriptItem::Message(ChatMessage::user("brand new prompt")),
        TranscriptItem::Message(ChatMessage::assistant("answer")),
    ];
    app.input = "unsent draft".to_string();
    let entry = CheckpointEntry {
        id: "cp-stale".to_string(),
        session_id: original_id.clone(),
        created_at_ms: 1,
        prompt_excerpt: "old prompt".to_string(),
        transcript_user_index: 8,
        parent_id: None,
        note: String::new(),
        files: Vec::new(),
    };

    app.fork_transcript_at_checkpoint(&entry);

    // Transcript untouched, draft preserved, session NOT forked.
    assert_eq!(app.transcript.len(), 2);
    assert_eq!(app.input, "unsent draft");
    assert_eq!(app.session.as_ref().unwrap().current_id(), original_id);
}

/// Finding [20] guardrail: a genuinely-live checkpoint (index maps to its
/// user message, excerpt still matches) still offers and applies fork.
#[test]
fn rewind_still_offers_fork_for_live_checkpoint() {
    let mut app = app();
    let workspace = app.tools.workspace().to_path_buf();
    let session = SessionStore::open(&workspace, SessionOpenMode::New).unwrap();
    let session_id = session.current_id();
    app.session = Some(session);
    app.transcript = vec![
        TranscriptItem::Message(ChatMessage::user("first prompt")),
        TranscriptItem::Message(ChatMessage::assistant("first answer")),
        TranscriptItem::Message(ChatMessage::user("second prompt")),
        TranscriptItem::Message(ChatMessage::assistant("second answer")),
    ];
    app.rewind_entries = vec![CheckpointEntry {
        id: "cp-live".to_string(),
        session_id,
        created_at_ms: 1,
        prompt_excerpt: "second prompt".to_string(),
        transcript_user_index: 2,
        parent_id: None,
        note: String::new(),
        files: Vec::new(),
    }];
    app.rewind_selection = 0;

    assert!(app.selected_rewind_offers_fork());
    assert!(
        app.rewind_confirm_options()
            .contains(&"Restore files + fork conversation")
    );
}

#[test]
fn edit_command_opens_picker_newest_first() {
    let mut app = app();
    app.transcript = vec![
        TranscriptItem::Message(ChatMessage::user("first prompt")),
        TranscriptItem::Message(ChatMessage::assistant("first answer")),
        TranscriptItem::Message(ChatMessage::user("second\nprompt with lines")),
        TranscriptItem::Message(ChatMessage::assistant("second answer")),
    ];

    assert!(app.run_local_tool_command("/edit"));

    assert_eq!(app.active_modal, Some(Modal::EditMessage));
    assert_eq!(app.edit_picker_selection, 0);
    assert_eq!(app.edit_picker_entries.len(), 2);
    assert_eq!(app.edit_picker_entries[0].transcript_index, 2);
    assert_eq!(
        app.edit_picker_entries[0].preview,
        "second prompt with lines"
    );
    assert_eq!(app.edit_picker_entries[1].transcript_index, 0);
    assert_eq!(app.edit_picker_entries[1].preview, "first prompt");
}

#[test]
fn edit_picker_caps_at_twenty_messages() {
    let mut app = app();
    app.transcript = (0..25)
        .map(|index| TranscriptItem::Message(ChatMessage::user(format!("prompt {index}"))))
        .collect();

    assert!(app.run_local_tool_command("/edit"));

    assert_eq!(app.edit_picker_entries.len(), EDIT_PICKER_LIMIT);
    assert_eq!(app.edit_picker_entries[0].preview, "prompt 24");
}

#[test]
fn edit_without_user_messages_stays_closed() {
    let mut app = app();
    app.transcript = vec![TranscriptItem::Message(ChatMessage::assistant("hi"))];

    assert!(app.run_local_tool_command("/edit"));

    assert_eq!(app.active_modal, None);
    assert_eq!(app.status_line, "no previous messages to edit");
}

#[test]
fn edit_is_refused_while_a_turn_is_streaming() {
    let mut app = app();
    app.transcript = vec![TranscriptItem::Message(ChatMessage::user("prompt"))];
    let (_sender, receiver) = mpsc::channel::<ModelStreamEvent>();
    app.model_events = Some(receiver);

    assert!(app.run_local_tool_command("/edit"));

    assert_eq!(app.active_modal, None);
    assert_eq!(
        app.status_line,
        "finish the current turn before editing a message"
    );
}

#[test]
fn edit_selection_forks_session_truncates_transcript_and_prefills_composer() {
    let mut app = app();
    let workspace = app.tools.workspace().to_path_buf();
    let session = SessionStore::open(&workspace, SessionOpenMode::New).unwrap();
    let original_id = session.current_id();
    let transcript = vec![
        TranscriptItem::Message(ChatMessage::user("first prompt")),
        TranscriptItem::Message(ChatMessage::assistant("first answer")),
        TranscriptItem::Message(ChatMessage::user("second prompt")),
        TranscriptItem::Message(ChatMessage::assistant("second answer")),
    ];
    session.save_transcript(&transcript).unwrap();
    app.session = Some(session);
    app.transcript = transcript;

    assert!(app.run_local_tool_command("/edit"));
    assert_eq!(app.active_modal, Some(Modal::EditMessage));
    // Newest first: selection 0 is "second prompt" at transcript index 2.
    app.handle_edit_message_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    assert_eq!(app.active_modal, None);
    assert_eq!(app.transcript.len(), 2);
    assert!(matches!(
        &app.transcript[1],
        TranscriptItem::Message(message) if message.content == "first answer"
    ));
    assert_eq!(app.input, "second prompt");
    assert_eq!(app.input_cursor, app.input_len());
    assert!(app.status_line.starts_with("editing message"));

    // The session tree gained a fork: the live session is a new child
    // of the original, and the original file still holds the full
    // four-item timeline.
    let session = app.session.as_ref().unwrap();
    let forked_id = session.current_id();
    assert_ne!(forked_id, original_id);
    assert_eq!(session.parent_id(), Some(original_id.as_str()));
    let original_path = workspace
        .join(".medusa")
        .join("sessions")
        .join(&original_id);
    let original: medusa_core::session::LoadedSessionFile<TranscriptItem> =
        read_session_file(&original_path).unwrap();
    assert_eq!(original.transcript.len(), 4);
}

#[test]
fn edit_selection_is_refused_when_a_turn_starts_while_picker_is_open() {
    let mut app = app();
    let workspace = app.tools.workspace().to_path_buf();
    let session = SessionStore::open(&workspace, SessionOpenMode::New).unwrap();
    app.session = Some(session);
    app.transcript = vec![TranscriptItem::Message(ChatMessage::user("prompt"))];

    assert!(app.run_local_tool_command("/edit"));
    let (_sender, receiver) = mpsc::channel::<ModelStreamEvent>();
    app.model_events = Some(receiver);
    app.handle_edit_message_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    assert_eq!(app.active_modal, None);
    assert_eq!(app.transcript.len(), 1);
    assert!(app.input.is_empty());
    assert_eq!(
        app.status_line,
        "finish the current turn before editing a message"
    );
}

#[test]
fn review_seeds_composer_without_sending() {
    let mut app = app();
    app.review_diff_check = |_| true;

    assert!(app.run_local_tool_command("/review"));

    assert_eq!(app.input, REVIEW_PROMPT_TEMPLATE);
    assert_eq!(app.input_cursor, app.input_len());
    assert!(app.input.contains("git status"));
    assert!(app.input.contains("git diff"));
    assert!(app.input.contains("correctness bugs first"));
    assert!(app.input.contains("file:line"));
    // Seeded only — nothing was sent and no turn started.
    assert!(app.transcript.is_empty());
    assert!(app.model_events.is_none());
    assert_eq!(
        app.status_line,
        "review prompt ready — edit and press enter"
    );
}

#[test]
fn review_toasts_when_nothing_to_review() {
    let mut app = app();
    app.review_diff_check = |_| false;

    assert!(app.run_local_tool_command("/review"));

    assert!(app.input.is_empty());
    assert_eq!(app.status_line, "nothing to review");
    assert_eq!(app.toast.as_ref().unwrap().message, "Nothing to review");
}

#[test]
fn reviewable_diff_probe_reports_repo_and_diff_state() {
    // Not a git repo: nothing to review.
    let bare = temp_workspace();
    assert!(!workspace_has_reviewable_diff(&bare));

    // Fresh repo with no changes at all: still nothing to review.
    let repo = temp_workspace();
    let init = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&repo)
        .status()
        .unwrap();
    assert!(init.success());
    assert!(!workspace_has_reviewable_diff(&repo));

    // A pending (untracked) file makes the workspace reviewable.
    fs::write(repo.join("pending.txt"), "change\n").unwrap();
    assert!(workspace_has_reviewable_diff(&repo));
}

#[test]
fn slash_prefix_suggests_resume() {
    let mut app = app();

    app.input = "/re".to_string();
    app.input_cursor = 3;

    let matches = app.slash_matches();
    assert!(matches.iter().any(|(command, _)| command.name == "/resume"));
}

#[test]
fn slash_prefix_suggests_tree() {
    let mut app = app();

    app.input = "/tr".to_string();
    app.input_cursor = 3;

    let matches = app.slash_matches();
    assert!(matches.iter().any(|(command, _)| command.name == "/tree"));
}

#[test]
fn slash_prefix_suggests_skills() {
    let mut app = app();

    app.input = "/sk".to_string();
    app.input_cursor = 3;

    let matches = app.slash_matches();
    assert!(matches.iter().any(|(command, _)| command.name == "/skills"));
}

#[test]
fn slash_prefix_suggests_workflow_commands() {
    let mut app = app();

    app.input = "/work".to_string();
    app.input_cursor = app.input_len();

    let matches = app.slash_matches();
    assert!(
        matches
            .iter()
            .any(|(command, _)| command.name == "/workflow")
    );
    assert!(
        matches
            .iter()
            .any(|(command, _)| command.name == "/workflows")
    );
}

#[test]
fn slash_search_matches_description_and_category() {
    let mut app = app();

    app.input = "/switch".to_string();
    app.input_cursor = app.input_len();

    let matches = app.slash_matches();
    assert!(matches.iter().any(|(command, _)| command.name == "/theme"));

    app.input = "/session".to_string();
    app.input_cursor = app.input_len();
    let matches = app.slash_matches();
    assert!(matches.iter().any(|(command, _)| command.name == "/resume"));
}

#[test]
fn command_palette_is_centered() {
    let rect = command_palette_rect(Rect::new(0, 0, 100, 40), 10);

    assert_eq!(rect.width, 92);
    assert_eq!(rect.height, 15);
    assert_eq!(rect.x, 4);
    assert_eq!(rect.y, 12);
}

#[test]
fn ctrl_p_opens_command_palette() {
    let mut app = app();

    app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));

    assert_eq!(app.input, "/");
    assert_eq!(app.input_cursor, 1);
    assert!(app.slash_suggestions_active());
}

#[test]
fn command_palette_navigation_uses_dedicated_keys() {
    let mut app = app();

    app.open_command_palette();
    app.handle_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
    assert!(app.slash_selection > 0);

    app.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
    assert_eq!(app.slash_selection, app.slash_matches().len() - 1);

    app.handle_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
    assert_eq!(app.slash_selection, 0);

    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(app.input, "");
    assert!(!app.should_quit);
}

#[test]
fn command_palette_tab_navigation_cycles_suggestions() {
    let mut app = app();

    app.open_command_palette();
    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(app.slash_selection, 1);

    app.handle_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
    assert_eq!(app.slash_selection, 0);
}

#[test]
fn enter_accepts_slash_suggestion() {
    let mut app = app();

    app.input = "/se".to_string();
    app.input_cursor = 3;
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    assert_eq!(app.active_modal, Some(Modal::Settings));
    assert_eq!(app.input, "");
}

#[test]
fn tools_command_lists_minimal_surface() {
    let mut app = app();

    app.input = "/tools".to_string();
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    assert!(matches!(
        &app.transcript[..],
        [TranscriptItem::Message(ChatMessage { role: ChatRole::System, content, .. })]
            if content.contains("terminal.exec") && content.contains("file.patch")
    ));
}

#[test]
fn reload_command_requests_restart() {
    let mut app = app();

    assert!(app.run_local_tool_command("/reload"));

    assert!(app.restart_requested);
    assert!(app.should_quit);
    assert_eq!(app.status_line, "reloading Medusa…");
}

#[test]
fn reload_command_refuses_active_work() {
    let mut app = app();
    let (_sender, receiver) = mpsc::channel();
    app.model_events = Some(receiver);

    assert!(app.run_local_tool_command("/reload"));

    assert!(!app.restart_requested);
    assert!(!app.should_quit);
    assert_eq!(app.status_line, "reload blocked: work is still running");
}

#[test]
fn reload_command_is_listed() {
    assert!(
        SLASH_COMMANDS
            .iter()
            .any(|command| command.name == "/reload")
    );
}

#[test]
fn reload_rebuild_probe_ignores_non_workspace_binary() {
    maybe_rebuild_before_reload(Path::new("/tmp/medusa-not-from-this-workspace"))
        .expect("non-workspace binaries should reload without a rebuild probe");
}

#[test]
fn themes_command_opens_theme_modal() {
    let mut app = app();

    app.input = "/theme".to_string();
    app.input_cursor = app.input_len();
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    assert_eq!(app.status_line, "themes opened");
    assert_eq!(app.active_modal, Some(Modal::Themes));
    assert_eq!(app.theme_selection, theme_index(app.theme));
}

#[test]
fn selecting_theme_from_palette_opens_theme_menu_without_input_arg() {
    let mut app = app();

    app.input = "/theme".to_string();
    app.input_cursor = app.input_len();
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    assert_eq!(app.active_modal, Some(Modal::Themes));
    assert_eq!(app.input, "");
    assert_eq!(app.status_line, "themes opened");
}

#[test]
fn theme_modal_can_apply_selection_with_keyboard() {
    let mut app = app();
    app.set_theme(ThemeKind::Medusa);

    app.open_themes_modal();
    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    assert_eq!(app.theme, ThemeKind::OpenCode);
    assert_eq!(app.active_modal, None);
    assert_eq!(app.status_line, "theme: opencode");
}

#[test]
fn theme_modal_previews_theme_while_navigating() {
    let mut app = app();
    app.theme = ThemeKind::Medusa;
    app.theme_selection = theme_index(app.theme);
    set_active_theme(app.theme);

    app.open_themes_modal();
    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));

    assert_eq!(app.theme, ThemeKind::OpenCode);
    assert_eq!(app.theme_preview_original, Some(ThemeKind::Medusa));
    assert_eq!(app.status_line, "preview theme: opencode");
}

#[test]
fn theme_preview_restyles_cached_chat_and_tool_rows() {
    let mut app = app();
    app.theme = ThemeKind::Medusa;
    app.theme_selection = theme_index(app.theme);
    set_active_theme(app.theme);
    app.transcript = vec![
        TranscriptItem::Message(ChatMessage::user("hello")),
        TranscriptItem::Tool(ToolRun {
            id: None,
            started_at: Instant::now(),
            pending_result: None,
            name: "terminal.exec".to_string(),
            summary: "$ cargo test".to_string(),
            state: ToolRunState::Succeeded,
            detail: "done".to_string(),
            expanded: false,
            group_expanded: false,
        }),
    ];
    app.attachment_previews.insert(
        "cached".to_string(),
        vec![Line::from(Span::styled("old", accent()))],
    );
    app.touch_transcript();

    app.visible_transcript_rows_cached();
    assert!(app.transcript_rows_cache.is_some());
    assert_eq!(
        app.transcript_rows_cache.as_ref().map(|cache| cache.theme),
        Some(ThemeKind::Medusa)
    );

    app.open_themes_modal();
    app.theme_selection = theme_index(ThemeKind::MaterialAmber);
    app.preview_theme_selection();

    assert!(app.transcript_rows_cache.is_none());
    assert!(app.last_transcript_rows.is_empty());
    assert!(app.attachment_previews.is_empty());

    let updated = app.visible_transcript_rows_cached();
    assert_eq!(
        app.transcript_rows_cache.as_ref().map(|cache| cache.theme),
        Some(ThemeKind::MaterialAmber)
    );
    assert!(first_span_fg_containing(&updated, "›").is_some());
    assert!(first_span_fg_containing(&updated, "terminal").is_some());
}

#[test]
fn theme_modal_escape_restores_previewed_theme() {
    let mut app = app();
    app.theme = ThemeKind::Medusa;
    app.theme_selection = theme_index(app.theme);
    set_active_theme(app.theme);

    app.open_themes_modal();
    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

    assert_eq!(app.theme, ThemeKind::Medusa);
    assert_eq!(app.theme_preview_original, None);
    assert_eq!(app.active_modal, None);
    assert_eq!(app.status_line, "closed");
}

#[test]
fn settings_modal_can_open_theme_editor_from_menu() {
    let mut app = app();

    app.open_settings_modal();
    app.settings_selection = app
        .settings_items()
        .iter()
        .position(|item| item.key == "theme")
        .unwrap();
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    assert_eq!(app.active_modal, Some(Modal::Themes));
    assert_eq!(app.theme_selection, theme_index(app.theme));
}

#[test]
fn theme_command_switches_active_theme() {
    let mut app = app();

    app.input = "/theme opencode".to_string();
    app.input_cursor = app.input_len();
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    assert_eq!(app.theme, ThemeKind::OpenCode);
    assert_eq!(app.status_line, "theme: opencode");
}

#[test]
fn theme_command_cycles_next_and_previous() {
    let (mut app, _workspace) = app_in_workspace();
    app.theme = ThemeKind::Medusa;
    app.theme_selection = theme_index(app.theme);
    set_active_theme(app.theme);

    assert!(app.run_local_tool_command("/theme next"));
    assert_eq!(app.theme, ThemeKind::OpenCode);
    assert_eq!(app.theme_selection, theme_index(ThemeKind::OpenCode));

    assert!(app.run_local_tool_command("/theme prev"));
    assert_eq!(app.theme, ThemeKind::Medusa);

    assert!(app.run_local_tool_command("/theme previous"));
    assert_eq!(app.theme, ThemeKind::Vesper);
    assert_eq!(app.status_line, "theme: vesper");
}

#[test]
fn slash_theme_prefix_suggests_theme_names() {
    let mut app = app();
    app.input = "/theme mat".to_string();
    app.input_cursor = app.input_len();

    let names = app
        .slash_matches()
        .into_iter()
        .map(|(command, _)| command.name)
        .collect::<Vec<_>>();

    assert!(names.contains(&"/theme material-dark"));
    assert!(names.contains(&"/theme material-amber"));
    assert!(!names.contains(&"/help"));
}

#[test]
fn theme_preview_lines_include_labeled_swatches() {
    let preview = theme_preview_lines(ThemeKind::MaterialTeal)
        .iter()
        .map(line_text)
        .collect::<Vec<_>>();

    for label in ["accent", "prompt", "tool", "success", "error"] {
        assert!(
            preview.iter().any(|line| line.contains(label)),
            "missing {label} swatch"
        );
    }
    assert!(
        preview
            .iter()
            .any(|line| line.contains("selection / focus"))
    );
    assert!(preview.iter().any(|line| line.contains("inline code")));
}

#[test]
fn additional_themes_are_listed_and_selectable() {
    let expected = [
        ("dracula", ThemeKind::Dracula),
        ("nord", ThemeKind::Nord),
        ("gruvbox", ThemeKind::Gruvbox),
        ("solarized-dark", ThemeKind::SolarizedDark),
        ("material-dark", ThemeKind::MaterialDark),
        ("material-teal", ThemeKind::MaterialTeal),
        ("material-amber", ThemeKind::MaterialAmber),
        ("material-indigo", ThemeKind::MaterialIndigo),
        ("material-rose", ThemeKind::MaterialRose),
    ];

    for (name, kind) in expected {
        assert!(ThemeKind::all().contains(&kind));
        assert_eq!(ThemeKind::from_name(name), Some(kind));
        assert_eq!(kind.name(), name);
    }

    assert_eq!(
        ThemeKind::from_name("material"),
        Some(ThemeKind::MaterialDark)
    );
    assert_eq!(
        ThemeKind::from_name("material-cyan"),
        Some(ThemeKind::MaterialTeal)
    );
    assert_eq!(
        ThemeKind::from_name("material-purple"),
        Some(ThemeKind::MaterialIndigo)
    );
    assert_eq!(
        ThemeKind::from_name("material-pink"),
        Some(ThemeKind::MaterialRose)
    );
}

#[test]
fn material_themes_use_distinct_accents() {
    let themes = [
        ThemeKind::MaterialDark,
        ThemeKind::MaterialTeal,
        ThemeKind::MaterialAmber,
        ThemeKind::MaterialIndigo,
        ThemeKind::MaterialRose,
    ];

    for theme in themes {
        let palette = theme.palette();
        assert_eq!(palette.success, MATERIAL_GREEN_400);
        assert_eq!(palette.error, MATERIAL_RED_400);
        assert_eq!(palette.separator, MATERIAL_BLUE_GREY_800);
    }

    assert_ne!(
        ThemeKind::MaterialTeal.palette().accent,
        ThemeKind::MaterialAmber.palette().accent
    );
}

#[test]
fn skills_command_lists_workspace_skills() {
    let workspace = temp_workspace();
    let skill_dir = workspace.join(".medusa/skills/review");
    fs::create_dir_all(&skill_dir).unwrap();
    fs::write(
        skill_dir.join("SKILL.md"),
        "description: Review code\n\nLead with findings.",
    )
    .unwrap();
    let session = SessionStore::open(&workspace, SessionOpenMode::New).unwrap();
    let mut app = App::build(false, Some(session));
    app.tools = ToolRuntime::new(&workspace).unwrap();

    app.input = "/skills".to_string();
    app.input_cursor = app.input_len();
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    assert_eq!(app.status_line, "skills listed");
    assert!(matches!(
        &app.transcript[..],
        [TranscriptItem::Message(ChatMessage { role: ChatRole::System, content, .. })]
            if content.contains("$review") && content.contains("Review code")
    ));
}

#[test]
fn agents_command_opens_modal_with_workspace_agents() {
    let workspace = temp_workspace();
    let dir = workspace.join(".medusa/agents");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("reviewer.md"),
        "name: reviewer\ndescription: Review diffs\ntools: read\n\nAlways lead with findings.",
    )
    .unwrap();
    let session = SessionStore::open(&workspace, SessionOpenMode::New).unwrap();
    let mut app = App::build(false, Some(session));
    app.tools = ToolRuntime::new(&workspace).unwrap();

    app.input = "/agents".to_string();
    app.input_cursor = app.input_len();
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    assert_eq!(app.active_modal, Some(Modal::Agents));
    assert_eq!(app.status_line, "named agents");
    assert_eq!(app.agent_registry.agents().len(), 1);
    assert_eq!(app.agent_registry.agents()[0].name, "reviewer");
    assert_eq!(
        app.agent_registry.agents()[0].tool_policy,
        SubagentToolPolicy::ReadOnly
    );

    // Esc closes through the generic modal fallback.
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(app.active_modal, None);
}

#[test]
fn agents_command_shows_empty_state_when_unconfigured() {
    let mut app = app();

    app.input = "/agents".to_string();
    app.input_cursor = app.input_len();
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    assert_eq!(app.active_modal, Some(Modal::Agents));
    assert!(app.agent_registry.is_empty());
}

#[test]
fn slash_prefix_suggests_mcp_command() {
    let mut app = app();

    app.input = "/mc".to_string();
    app.input_cursor = 3;

    let matches = app.slash_matches();
    assert!(matches.iter().any(|(command, _)| command.name == "/mcp"));
}

#[test]
fn mcp_command_opens_modal_with_config_hint_when_unconfigured() {
    let mut app = app();

    app.input = "/mcp".to_string();
    app.input_cursor = app.input_len();
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    assert_eq!(app.active_modal, Some(Modal::Mcp));
    assert!(app.mcp_statuses.is_empty());
    assert_eq!(app.status_line, "mcp servers");

    // Esc closes through the generic modal fallback.
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(app.active_modal, None);
}

#[test]
fn mcp_command_snapshots_configured_servers_without_starting_them() {
    let workspace = temp_workspace();
    fs::create_dir_all(workspace.join(".medusa")).unwrap();
    fs::write(
        workspace.join(".medusa/mcp.json"),
        r#"{"servers":{"docs":{"command":"python3","args":["server.py"],"readOnly":true}}}"#,
    )
    .unwrap();
    let session = SessionStore::open(&workspace, SessionOpenMode::New).unwrap();
    let mut app = App::build(false, Some(session));
    app.mcp = McpRegistry::load(&workspace).unwrap();
    app.tools = ToolRuntime::new(&workspace)
        .unwrap()
        .with_mcp(app.mcp.clone());

    app.input = "/mcp".to_string();
    app.input_cursor = app.input_len();
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    assert_eq!(app.active_modal, Some(Modal::Mcp));
    assert_eq!(app.mcp_statuses.len(), 1);
    assert_eq!(app.mcp_statuses[0].name, "docs");
    assert_eq!(app.mcp_statuses[0].state, McpServerStateLabel::Idle);
    assert!(app.mcp_statuses[0].read_only);
    assert_eq!(app.mcp_statuses[0].command_line, "python3 server.py");
}

#[test]
fn mcp_restart_validates_the_server_name() {
    let mut app = app();

    app.input = "/mcp restart nope".to_string();
    app.input_cursor = app.input_len();
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    assert_eq!(app.status_line, "unknown mcp server");
    assert_eq!(app.active_modal, None);

    app.input = "/mcp bogus".to_string();
    app.input_cursor = app.input_len();
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    assert_eq!(app.status_line, "usage: /mcp [restart <server>]");
}

#[test]
fn unknown_slash_command_does_not_hit_model() {
    let mut app = app();

    app.input = "/nope".to_string();
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    assert_eq!(app.status_line, "unknown command");
    assert!(matches!(
        &app.transcript[..],
        [TranscriptItem::Message(ChatMessage { role: ChatRole::System, content, .. })]
            if content.contains("unknown command: /nope")
    ));
}

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

    let chars = transcript_char_usage(&app.transcript);
    assert_eq!(chars.total(), app.context_usage_chars());
    assert!(chars.messages > 0);
    assert!(chars.tool_outputs > 0);
    assert!(chars.reasoning > 0);
    assert!(chars.plans > 0);

    let report = app.build_context_report();
    // Transcript categories reuse the same ~4 chars/token estimate.
    assert_eq!(report.message_tokens, chars.messages.div_ceil(4));
    assert_eq!(report.tool_tokens, chars.tool_outputs.div_ceil(4));
    assert_eq!(report.reasoning_tokens, chars.reasoning.div_ceil(4));
    assert_eq!(report.plan_tokens, chars.plans.div_ceil(4));
    // The report's total is exactly the sum of its categories.
    assert_eq!(
        report.total_tokens(),
        report.instructions_tokens
            + report.system_tokens
            + report.message_tokens
            + report.tool_tokens
            + report.reasoning_tokens
            + report.plan_tokens
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
    assert!(!text.iter().any(|line| line.contains("thinking")));
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

fn type_chars(app: &mut App, text: &str) {
    for ch in text.chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
    }
}

#[test]
fn collect_workspace_files_skips_junk_dirs_and_respects_cap() {
    let workspace = temp_workspace();
    fs::create_dir_all(workspace.join("src")).unwrap();
    fs::create_dir_all(workspace.join(".git")).unwrap();
    fs::create_dir_all(workspace.join("target/debug")).unwrap();
    fs::create_dir_all(workspace.join("node_modules/pkg")).unwrap();
    fs::write(workspace.join("README.md"), "x").unwrap();
    fs::write(workspace.join("src/main.rs"), "x").unwrap();
    fs::write(workspace.join(".git/config"), "x").unwrap();
    fs::write(workspace.join("target/debug/junk"), "x").unwrap();
    fs::write(workspace.join("node_modules/pkg/index.js"), "x").unwrap();

    let files = collect_workspace_files(&workspace, MENTION_FILE_WALK_CAP);
    assert_eq!(
        files,
        vec!["README.md".to_string(), "src/main.rs".to_string()]
    );

    for index in 0..10 {
        fs::write(workspace.join(format!("file{index}.txt")), "x").unwrap();
    }
    assert_eq!(collect_workspace_files(&workspace, 3).len(), 3);
}

#[test]
fn mention_match_prefers_file_name_then_segment_then_substring() {
    let (name_score, positions) = mention_match("crates/medusa-tui/src/main.rs", "main").unwrap();
    assert_eq!(name_score, 0);
    assert_eq!(positions, (22..26).collect::<Vec<_>>());

    let (segment_score, _) = mention_match("crates/medusa-tui/src/main.rs", "medusa").unwrap();
    assert_eq!(segment_score, 1);

    let (substring_score, _) = mention_match("docs/comments.md", "men").unwrap();
    assert_eq!(substring_score, 2);

    let (subsequence_score, _) = mention_match("src/handlers.rs", "hdl").unwrap();
    assert_eq!(subsequence_score, 3);

    assert!(mention_match("src/lib.rs", "zzz").is_none());
}

#[test]
fn typing_at_token_opens_mention_picker_and_enter_inserts_path() {
    let (mut app, workspace) = app_in_workspace();
    fs::create_dir_all(workspace.join("src")).unwrap();
    fs::write(workspace.join("src/lib.rs"), "x").unwrap();
    fs::write(workspace.join("README.md"), "x").unwrap();

    type_chars(&mut app, "look at @li");
    assert!(app.mention_popup_visible());
    let matches = app.mention_matches();
    assert_eq!(matches[0].0, "src/lib.rs");

    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(app.input, "look at src/lib.rs ");
    assert_eq!(app.input_cursor, app.input_len());
    assert!(!app.mention_popup_visible());
    // The transcript got no user message: Enter completed, not submitted.
    assert!(app.transcript.is_empty());
}

#[test]
fn tab_completes_mention_and_up_down_navigate() {
    let mut app = app();
    // Pre-seeding the candidate list keeps the walk out of this test:
    // refresh_mention_state only loads files when none are cached.
    app.mention_files = Some(vec![
        "src/alpha.rs".to_string(),
        "src/alpine.rs".to_string(),
    ]);
    type_chars(&mut app, "@alp");
    assert!(app.mention_popup_visible());
    assert_eq!(app.mention_selection, 0);

    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(app.mention_selection, 1);
    app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(app.mention_selection, 0);

    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(app.input, "src/alpha.rs ");
}

#[test]
fn escape_dismisses_mention_picker_without_clearing_input() {
    let (mut app, workspace) = app_in_workspace();
    fs::write(workspace.join("notes.txt"), "x").unwrap();

    type_chars(&mut app, "@not");
    assert!(app.mention_popup_visible());

    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(!app.mention_popup_visible());
    assert_eq!(app.input, "@not");

    // Typing again reopens the picker.
    type_chars(&mut app, "e");
    assert!(app.mention_popup_visible());
}

#[test]
fn mention_picker_defers_to_slash_suggestions() {
    let mut app = app();
    app.mention_files = Some(vec!["help.md".to_string()]);
    type_chars(&mut app, "/he");
    assert!(app.slash_suggestions_active());
    assert!(!app.mention_popup_visible());
    assert!(app.mention_matches().is_empty());
}

#[test]
fn mention_picker_wins_over_decision_until_dismissed() {
    let (mut app, workspace) = app_in_workspace();
    fs::write(workspace.join("plan.md"), "x").unwrap();
    app.apply_decision_request_output(
        r#"{"title":"pick","reason":"","questions":[{"id":"q1","prompt":"which?","kind":"text"}]}"#,
    )
    .unwrap();
    assert!(app.pending_decision().is_some());

    type_chars(&mut app, "@pla");
    assert!(app.mention_popup_visible());

    // Enter completes the mention, not the decision answer.
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(app.input, "plan.md ");
    assert!(app.pending_decision().is_some());

    // With the picker gone, Enter answers the decision with the text.
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(app.pending_decision().is_none());
}

#[test]
fn mention_picker_leaves_multiline_navigation_alone() {
    let (mut app, workspace) = app_in_workspace();
    fs::write(workspace.join("data.csv"), "x").unwrap();

    app.input = "first\nsecond".to_string();
    app.input_cursor = app.input_len();
    assert!(!app.mention_popup_visible());
    app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(app.input_cursor, 5); // moved to line 1, column 5

    // With an active mention token, Up drives the picker instead.
    app.input_cursor = app.input_len();
    type_chars(&mut app, " @dat");
    assert!(app.mention_popup_visible());
    let cursor_before = app.input_cursor;
    app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(app.input_cursor, cursor_before);
}

#[test]
fn quick_memory_content_creates_file_section_and_appends() {
    let created = quick_memory_content("", "use rg not grep");
    assert_eq!(
        created,
        "# Project notes\n\n## Notes\n\n- use rg not grep\n"
    );

    let appended = quick_memory_content(&created, "tests live in-module");
    assert_eq!(
        appended,
        "# Project notes\n\n## Notes\n\n- use rg not grep\n- tests live in-module\n"
    );

    let with_following_section = "# Repo\n\n## Notes\n\n- old note\n\n## Commands\n\ncargo test\n";
    let inserted = quick_memory_content(with_following_section, "new note");
    assert_eq!(
        inserted,
        "# Repo\n\n## Notes\n\n- old note\n- new note\n\n## Commands\n\ncargo test\n"
    );

    let no_section = "# Repo\n\nSome intro.\n";
    let grown = quick_memory_content(no_section, "first note");
    assert_eq!(grown, "# Repo\n\nSome intro.\n\n## Notes\n\n- first note\n");
}

/// [22]: a multi-line quick-memory note containing a markdown heading must
/// not forge a new AGENTS.md section or scramble later insertions.
#[test]
fn quick_memory_neutralizes_multiline_heading_notes() {
    let existing = "# Repo\n\n## Notes\n\n- old note\n";
    let note = "deploy steps\n## Deploy\nrun make ship";
    let content = quick_memory_content(existing, note);

    // No line inside the file is a forged heading carrying the pasted text.
    assert!(
        !content
            .lines()
            .any(|line| line.trim_start().starts_with('#') && line.contains("Deploy")),
        "pasted heading forged a section:\n{content}"
    );
    // The note collapsed to a single bullet under Notes.
    assert!(
        content.contains("- deploy steps ## Deploy run make ship"),
        "note not collapsed to one bullet:\n{content}"
    );

    // A later note still lands under the same Notes section, in order, and
    // no rogue standalone "## Deploy" heading exists to break the scan.
    let next = quick_memory_content(&content, "second note");
    let notes_at = next.find("## Notes").unwrap();
    let old_at = next.find("- old note").unwrap();
    let first_at = next.find("- deploy steps").unwrap();
    let second_at = next.find("- second note").unwrap();
    assert!(
        notes_at < old_at && old_at < first_at && first_at < second_at,
        "later note escaped the Notes section:\n{next}"
    );
    assert!(
        !next.lines().any(|line| line.trim() == "## Deploy"),
        "a standalone Deploy heading was forged:\n{next}"
    );
}

/// [22]: a note that *starts* with a heading marker has it neutralized so
/// the bullet can never itself read as a heading.
#[test]
fn quick_memory_strips_leading_heading_marker() {
    let content = quick_memory_content("", "## Deploy\nrun make ship");
    assert!(
        content.contains("- Deploy run make ship"),
        "leading heading marker not neutralized:\n{content}"
    );
    assert!(
        !content.lines().any(|line| line.trim() == "## Deploy"),
        "leading heading marker forged a section:\n{content}"
    );
}

#[test]
fn hash_note_writes_agents_md_without_sending_a_turn() {
    let (mut app, workspace) = app_in_workspace();
    app.input = "# always run clippy before finishing".to_string();
    app.input_cursor = app.input_len();

    app.submit_input();

    let content = fs::read_to_string(workspace.join("AGENTS.md")).unwrap();
    assert!(content.contains("## Notes"));
    assert!(content.contains("- always run clippy before finishing"));

    // Nothing went to the model: no user message, no worker channel.
    assert!(app.model_events.is_none());
    assert!(!app.transcript.iter().any(|item| matches!(
        item,
        TranscriptItem::Message(ChatMessage {
            role: ChatRole::User,
            ..
        })
    )));

    // Muted system line explains when the note takes effect.
    let Some(TranscriptItem::Message(message)) = app.transcript.last() else {
        panic!("expected transcript line");
    };
    assert_eq!(message.role, ChatRole::System);
    assert!(message.content.contains("applies from next turn"));

    assert_eq!(app.input, "");
    let toast = app.toast.as_ref().expect("toast");
    assert_eq!(toast.message, "noted in AGENTS.md");
    assert_eq!(toast.kind, ToastKind::Success);

    // The per-turn project-instructions loader picks the note up, which
    // is what carries it to the model next turn.
    let context = medusa_core::project::project_instructions_context(&workspace).unwrap();
    assert!(context.contains("always run clippy before finishing"));

    // A second note appends instead of duplicating the section.
    app.input = "# prefer eyre::Result".to_string();
    app.input_cursor = app.input_len();
    app.submit_input();
    let content = fs::read_to_string(workspace.join("AGENTS.md")).unwrap();
    assert_eq!(content.matches("## Notes").count(), 1);
    assert!(content.contains("- prefer eyre::Result"));
}

#[test]
fn bell_gating_requires_enabled_and_long_turn() {
    assert!(!should_ring_bell(true, None));
    assert!(!should_ring_bell(true, Some(Duration::from_secs(3))));
    assert!(!should_ring_bell(false, Some(Duration::from_secs(30))));
    assert!(should_ring_bell(true, Some(Duration::from_secs(11))));
}

#[test]
fn bell_env_override_beats_setting() {
    assert!(bell_enabled(true, None));
    assert!(!bell_enabled(false, None));
    assert!(!bell_enabled(true, Some("off")));
    assert!(!bell_enabled(true, Some("OFF")));
    assert!(!bell_enabled(true, Some("0")));
    assert!(bell_enabled(false, Some("on")));
    assert!(bell_enabled(true, Some("unrecognized")));
}

#[test]
fn bell_preference_round_trips_through_settings() {
    let workspace = temp_workspace();
    save_bell_preference(&workspace, false).unwrap();
    let settings = load_app_settings(&workspace).unwrap();
    assert_eq!(settings.bell, Some(false));
}
