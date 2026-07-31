use super::*;

fn turn_recorder(workspace: &Path) -> CheckpointRecorder {
    CheckpointRecorder::new(
        workspace,
        crate::checkpoint::CheckpointMeta {
            session_id: "session-test.json".to_string(),
            prompt_excerpt: "test turn".to_string(),
            transcript_user_index: 0,
        },
    )
}

#[test]
fn file_edit_captures_pre_image_before_write() {
    let workspace = temp_workspace();
    fs::write(workspace.join("hello.txt"), "old\n").unwrap();
    let recorder = turn_recorder(&workspace);
    let runtime = ToolRuntime::new(&workspace)
        .unwrap()
        .with_checkpoint_recorder(recorder.clone());

    runtime
        .file_edit(FileEditRequest::new("hello.txt", "old\n", "new\n"))
        .unwrap();

    let summary = recorder.finish().unwrap();
    assert_eq!(summary.file_count, 1);
    let stored = workspace
        .join(".medusa/checkpoints")
        .join(&summary.id)
        .join("files/hello.txt");
    assert_eq!(fs::read_to_string(stored).unwrap(), "old\n");

    // Round trip: restore rewinds the edit.
    crate::checkpoint::CheckpointStore::open(&workspace)
        .unwrap()
        .restore(&summary.id)
        .unwrap();
    assert_eq!(
        fs::read_to_string(workspace.join("hello.txt")).unwrap(),
        "old\n"
    );
}

#[test]
fn file_edit_created_file_records_absent_pre_image() {
    let workspace = temp_workspace();
    let recorder = turn_recorder(&workspace);
    let runtime = ToolRuntime::new(&workspace)
        .unwrap()
        .with_checkpoint_recorder(recorder.clone());

    runtime
        .file_edit(FileEditRequest::new("src/new.txt", "", "hello\n"))
        .unwrap();

    let summary = recorder.finish().unwrap();
    crate::checkpoint::CheckpointStore::open(&workspace)
        .unwrap()
        .restore(&summary.id)
        .unwrap();
    assert!(!workspace.join("src/new.txt").exists());
}

#[test]
fn file_patch_codex_move_captures_source_and_destination() {
    let workspace = temp_workspace();
    fs::write(workspace.join("move-me.txt"), "move\n").unwrap();
    let recorder = turn_recorder(&workspace);
    let runtime = ToolRuntime::new(&workspace)
        .unwrap()
        .with_checkpoint_recorder(recorder.clone());

    let diff = r#"*** Begin Patch
*** Update File: move-me.txt
*** Move to: moved.txt
*** End Patch
"#;
    runtime.file_patch(FilePatchRequest::new(diff)).unwrap();
    assert!(!workspace.join("move-me.txt").exists());
    assert!(workspace.join("moved.txt").exists());

    let summary = recorder.finish().unwrap();
    assert_eq!(summary.file_count, 2);
    crate::checkpoint::CheckpointStore::open(&workspace)
        .unwrap()
        .restore(&summary.id)
        .unwrap();
    assert_eq!(
        fs::read_to_string(workspace.join("move-me.txt")).unwrap(),
        "move\n"
    );
    assert!(!workspace.join("moved.txt").exists());
}

/// A 100%-similarity git rename (no `---`/`+++` hunks) must capture the
/// rename SOURCE so rewind can recreate it; before the fix only the
/// destination was captured and rewind made the file vanish. Regression
/// for finding [11].
#[test]
fn file_patch_pure_rename_captures_source_and_restores_it() {
    let workspace = temp_workspace();
    fs::write(workspace.join("old.rs"), "fn main() {}\n").unwrap();
    let recorder = turn_recorder(&workspace);
    let runtime = ToolRuntime::new(&workspace)
        .unwrap()
        .with_checkpoint_recorder(recorder.clone());

    let diff = "diff --git a/old.rs b/new.rs\nsimilarity index 100%\nrename from old.rs\nrename to new.rs\n";
    let result = runtime.file_patch(FilePatchRequest::new(diff)).unwrap();

    assert!(!workspace.join("old.rs").exists());
    assert!(workspace.join("new.rs").exists());
    // Both the source and destination appear in the approval/changed list.
    assert!(result.changed_files.contains(&"old.rs".to_string()));
    assert!(result.changed_files.contains(&"new.rs".to_string()));

    let summary = recorder.finish().unwrap();
    assert_eq!(summary.file_count, 2);

    crate::checkpoint::CheckpointStore::open(&workspace)
        .unwrap()
        .restore(&summary.id)
        .unwrap();
    // Rewind recreates the moved-from file with its original content and
    // removes the moved-to file.
    assert!(
        workspace.join("old.rs").exists(),
        "rewind must recreate the rename source"
    );
    assert_eq!(
        fs::read_to_string(workspace.join("old.rs")).unwrap(),
        "fn main() {}\n"
    );
    assert!(!workspace.join("new.rs").exists());
}

/// file_edit must refuse an existing target reached through an
/// out-of-workspace symlink WITHOUT first capturing a pre-image (which
/// would copy the host file into `.medusa` and poison the manifest).
/// Regression for finding [10].
#[cfg(unix)]
#[test]
fn file_edit_through_out_of_workspace_symlink_captures_nothing() {
    use std::os::unix::fs::symlink;

    let workspace = temp_workspace();
    let outside = temp_workspace();
    fs::write(outside.join(".gitconfig"), "[user] host = secret\n").unwrap();
    symlink(&outside, workspace.join("home")).unwrap();

    let recorder = turn_recorder(&workspace);
    let runtime = ToolRuntime::new(&workspace)
        .unwrap()
        .with_checkpoint_recorder(recorder.clone());

    let error = runtime
        .file_edit(FileEditRequest::new(
            "home/.gitconfig",
            "[user] host = secret\n",
            "[user] host = evil\n",
        ))
        .unwrap_err();
    assert!(
        error.to_string().contains("escapes workspace"),
        "expected an escape refusal, got: {error}"
    );

    // Nothing captured: no pre-image, no checkpoint dir, host file intact.
    assert!(recorder.finish().is_none());
    assert!(!workspace.join(".medusa/checkpoints").exists());
    assert_eq!(
        fs::read_to_string(outside.join(".gitconfig")).unwrap(),
        "[user] host = secret\n"
    );
}

/// file_patch must refuse a patch path reached through an out-of-workspace
/// symlink before capture snapshots it or git apply writes through it.
/// General-case defense for finding [10] ("same audit for file_patch").
#[cfg(unix)]
#[test]
fn file_patch_through_out_of_workspace_symlink_captures_nothing() {
    use std::os::unix::fs::symlink;

    let workspace = temp_workspace();
    let outside = temp_workspace();
    fs::write(outside.join("secret.txt"), "host\n").unwrap();
    symlink(&outside, workspace.join("home")).unwrap();

    let recorder = turn_recorder(&workspace);
    let runtime = ToolRuntime::new(&workspace)
        .unwrap()
        .with_checkpoint_recorder(recorder.clone());

    let diff = "--- a/home/secret.txt\n+++ b/home/secret.txt\n@@ -1 +1 @@\n-host\n+evil\n";
    let error = runtime.file_patch(FilePatchRequest::new(diff)).unwrap_err();
    assert!(
        error.to_string().contains("symlink") || error.to_string().contains("escapes"),
        "expected an escape refusal, got: {error}"
    );

    assert!(recorder.finish().is_none());
    assert!(!workspace.join(".medusa/checkpoints").exists());
    assert_eq!(
        fs::read_to_string(outside.join("secret.txt")).unwrap(),
        "host\n"
    );
}

#[test]
fn denied_approval_leaves_no_checkpoint() {
    let workspace = ask_workspace();
    fs::write(workspace.join("hello.txt"), "old\n").unwrap();
    let recorder = turn_recorder(&workspace);
    let runtime = ToolRuntime::new(&workspace)
        .unwrap()
        .with_approval_handler(Arc::new(|_request: ApprovalRequest| ApprovalDecision::Deny))
        .with_checkpoint_recorder(recorder.clone());

    let error = runtime
        .file_edit(FileEditRequest::new("hello.txt", "old\n", "new\n"))
        .unwrap_err();
    assert!(error.to_string().contains("denied"));

    assert!(recorder.finish().is_none());
    assert!(!workspace.join(".medusa/checkpoints").exists());
}

#[test]
fn checkpoint_capture_failure_fails_file_edit_and_leaves_target_untouched() {
    let workspace = temp_workspace();
    fs::write(workspace.join("hello.txt"), "old\n").unwrap();
    // A regular file blocks creation of the checkpoints directory.
    fs::create_dir_all(workspace.join(".medusa")).unwrap();
    fs::write(workspace.join(".medusa/checkpoints"), "not a directory").unwrap();
    let runtime = ToolRuntime::new(&workspace)
        .unwrap()
        .with_checkpoint_recorder(turn_recorder(&workspace));

    let error = runtime
        .file_edit(FileEditRequest::new("hello.txt", "old\n", "new\n"))
        .unwrap_err();

    assert!(error.to_string().contains("checkpoint"), "{error:?}");
    assert_eq!(
        fs::read_to_string(workspace.join("hello.txt")).unwrap(),
        "old\n"
    );
}
