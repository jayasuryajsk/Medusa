use super::*;

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
