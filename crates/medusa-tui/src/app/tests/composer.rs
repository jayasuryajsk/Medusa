use super::*;

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

#[test]
fn reasoning_visibility_round_trips_through_settings() {
    let workspace = temp_workspace();
    assert!(!load_app_settings(&workspace).unwrap().show_reasoning());

    save_reasoning_visibility(&workspace, true).unwrap();
    assert!(load_app_settings(&workspace).unwrap().show_reasoning());
}
