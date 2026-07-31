use super::*;

fn temp_workspace() -> PathBuf {
    static TEMP_COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let index = TEMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let pid = std::process::id();
    let path = std::env::temp_dir().join(format!("medusa-checkpoint-test-{pid}-{suffix}-{index}"));
    fs::create_dir_all(&path).unwrap();
    path.canonicalize().unwrap()
}

fn meta(prompt: &str, user_index: usize) -> CheckpointMeta {
    CheckpointMeta {
        session_id: "session-test.json".to_string(),
        prompt_excerpt: prompt.to_string(),
        transcript_user_index: user_index,
    }
}

fn recorder(workspace: &Path, prompt: &str) -> CheckpointRecorder {
    CheckpointRecorder::new(workspace, meta(prompt, 0))
}

#[test]
fn capture_and_restore_round_trip_restores_original_bytes() {
    let workspace = temp_workspace();
    fs::write(workspace.join("f.txt"), "original\n").unwrap();

    let turn = recorder(&workspace, "edit f");
    turn.capture(&["f.txt".to_string()]).unwrap();
    fs::write(workspace.join("f.txt"), "mutated\n").unwrap();
    let summary = turn.finish().unwrap();
    assert_eq!(summary.file_count, 1);

    let store = CheckpointStore::open(&workspace).unwrap();
    let report = store.restore(&summary.id).unwrap();

    assert_eq!(report.restored, vec!["f.txt".to_string()]);
    assert_eq!(
        fs::read_to_string(workspace.join("f.txt")).unwrap(),
        "original\n"
    );
}

#[test]
fn absent_pre_image_deletes_created_file_and_cleans_empty_dirs() {
    let workspace = temp_workspace();

    let turn = recorder(&workspace, "create nested file");
    turn.capture(&["deep/nested/new.txt".to_string()]).unwrap();
    fs::create_dir_all(workspace.join("deep/nested")).unwrap();
    fs::write(workspace.join("deep/nested/new.txt"), "made this turn\n").unwrap();
    let summary = turn.finish().unwrap();

    let store = CheckpointStore::open(&workspace).unwrap();
    let report = store.restore(&summary.id).unwrap();

    assert_eq!(report.deleted, vec!["deep/nested/new.txt".to_string()]);
    assert!(!workspace.join("deep/nested/new.txt").exists());
    assert!(!workspace.join("deep").exists(), "empty parents cleaned");
}

#[test]
fn composition_across_checkpoints_restores_earliest_pre_images() {
    let workspace = temp_workspace();
    fs::write(workspace.join("f.txt"), "pre-A\n").unwrap();

    let turn_a = recorder(&workspace, "turn A");
    turn_a.capture(&["f.txt".to_string()]).unwrap();
    fs::write(workspace.join("f.txt"), "pre-B\n").unwrap();
    let checkpoint_a = turn_a.finish().unwrap();

    let turn_b = recorder(&workspace, "turn B");
    turn_b
        .capture(&["f.txt".to_string(), "g.txt".to_string()])
        .unwrap();
    fs::write(workspace.join("f.txt"), "post-B\n").unwrap();
    fs::write(workspace.join("g.txt"), "created in B\n").unwrap();
    let checkpoint_b = turn_b.finish().unwrap();

    let store = CheckpointStore::open(&workspace).unwrap();

    // restore(B): only turn B is undone.
    store.restore(&checkpoint_b.id).unwrap();
    assert_eq!(
        fs::read_to_string(workspace.join("f.txt")).unwrap(),
        "pre-B\n"
    );
    assert!(!workspace.join("g.txt").exists());

    // Redo the mutations, then restore(A): both turns are undone.
    fs::write(workspace.join("f.txt"), "post-B\n").unwrap();
    fs::write(workspace.join("g.txt"), "created in B\n").unwrap();
    let report = store.restore(&checkpoint_a.id).unwrap();
    assert_eq!(
        fs::read_to_string(workspace.join("f.txt")).unwrap(),
        "pre-A\n"
    );
    assert!(!workspace.join("g.txt").exists());
    assert!(report.safety_checkpoint.is_some());
}

#[test]
fn capture_is_first_write_wins_per_path() {
    let workspace = temp_workspace();
    fs::write(workspace.join("f.txt"), "first\n").unwrap();

    let turn = recorder(&workspace, "double edit");
    turn.capture(&["f.txt".to_string()]).unwrap();
    fs::write(workspace.join("f.txt"), "second\n").unwrap();
    // Second capture of the same path must not overwrite the pre-image.
    turn.capture(&["f.txt".to_string()]).unwrap();
    fs::write(workspace.join("f.txt"), "third\n").unwrap();
    let summary = turn.finish().unwrap();
    assert_eq!(summary.file_count, 1);

    CheckpointStore::open(&workspace)
        .unwrap()
        .restore(&summary.id)
        .unwrap();
    assert_eq!(
        fs::read_to_string(workspace.join("f.txt")).unwrap(),
        "first\n"
    );
}

#[test]
fn finish_returns_none_and_creates_no_dir_for_read_only_turns() {
    let workspace = temp_workspace();
    let turn = recorder(&workspace, "read only");

    assert!(turn.finish().is_none());
    assert!(!checkpoints_dir(&workspace).exists());
}

#[test]
fn medusa_internal_paths_are_never_captured() {
    let workspace = temp_workspace();
    fs::create_dir_all(workspace.join(".medusa/sessions")).unwrap();
    fs::write(workspace.join(".medusa/sessions/s.json"), "{}").unwrap();

    let turn = recorder(&workspace, "internal write");
    turn.capture(&[".medusa/sessions/s.json".to_string(), ".medusa".to_string()])
        .unwrap();

    assert!(turn.finish().is_none());
    assert!(!checkpoints_dir(&workspace).exists());
}

#[test]
fn capture_rejects_escaping_paths() {
    let workspace = temp_workspace();
    let turn = recorder(&workspace, "escape attempt");

    assert!(turn.capture(&["../outside.txt".to_string()]).is_err());
    assert!(turn.capture(&["/etc/hosts".to_string()]).is_err());
}

#[test]
fn medusa_internal_detects_curdir_prefixed_paths() {
    // Raw string prefix checks miss `./` — a crafted manifest path like
    // `./.medusa/checkpoints/x/manifest.json` must still be treated as
    // internal so restore refuses to overwrite another checkpoint's data.
    assert!(is_medusa_internal(".medusa"));
    assert!(is_medusa_internal(".medusa/sessions/s.json"));
    assert!(is_medusa_internal("./.medusa/checkpoints/x/manifest.json"));
    assert!(is_medusa_internal(".//.medusa/x"));
    assert!(is_medusa_internal("./././.medusa/x"));
    assert!(!is_medusa_internal("src/.medusa_helper.rs"));
    assert!(!is_medusa_internal("notes.md"));
}

#[test]
fn capture_fails_closed_when_checkpoint_dir_is_unwritable() {
    let workspace = temp_workspace();
    fs::write(workspace.join("f.txt"), "content\n").unwrap();
    // A regular file where the checkpoints dir must go blocks creation.
    fs::create_dir_all(workspace.join(".medusa")).unwrap();
    fs::write(checkpoints_dir(&workspace), "not a directory").unwrap();

    let turn = recorder(&workspace, "blocked");
    let error = turn.capture(&["f.txt".to_string()]).unwrap_err();

    assert!(
        error.to_string().contains("checkpoint"),
        "error should name the checkpoint dir: {error:?}"
    );
}

#[test]
fn restore_refuses_on_broken_chain() {
    let workspace = temp_workspace();
    fs::write(workspace.join("f.txt"), "v1\n").unwrap();

    let mut ids = Vec::new();
    for version in ["v2", "v3", "v4"] {
        let turn = recorder(&workspace, version);
        turn.capture(&["f.txt".to_string()]).unwrap();
        fs::write(workspace.join("f.txt"), format!("{version}\n")).unwrap();
        ids.push(turn.finish().unwrap().id);
    }

    // Manually delete the middle checkpoint: the chain to the oldest one
    // is now broken.
    fs::remove_dir_all(checkpoints_dir(&workspace).join(&ids[1])).unwrap();

    let store = CheckpointStore::open(&workspace).unwrap();
    let error = store.restore(&ids[0]).unwrap_err();
    assert!(error.to_string().contains("chain"), "{error:?}");

    // The newest checkpoint alone is still restorable.
    store.restore(&ids[2]).unwrap();
    assert_eq!(fs::read_to_string(workspace.join("f.txt")).unwrap(), "v3\n");
}

#[test]
fn restore_creates_pre_rewind_safety_checkpoint() {
    let workspace = temp_workspace();
    fs::write(workspace.join("f.txt"), "before\n").unwrap();

    let turn = recorder(&workspace, "edit");
    turn.capture(&["f.txt".to_string()]).unwrap();
    fs::write(workspace.join("f.txt"), "after\n").unwrap();
    let summary = turn.finish().unwrap();

    let store = CheckpointStore::open(&workspace).unwrap();
    let report = store.restore(&summary.id).unwrap();
    let safety_id = report.safety_checkpoint.unwrap();

    // The safety checkpoint holds the pre-restore ("after") content, so
    // restoring it undoes the rewind.
    assert_eq!(
        fs::read_to_string(workspace.join("f.txt")).unwrap(),
        "before\n"
    );
    store.restore(&safety_id).unwrap();
    assert_eq!(
        fs::read_to_string(workspace.join("f.txt")).unwrap(),
        "after\n"
    );
}

#[test]
fn prune_keeps_newest_and_respects_byte_budget() {
    let workspace = temp_workspace();
    fs::write(workspace.join("f.txt"), "x".repeat(1024)).unwrap();

    let mut ids = Vec::new();
    for index in 0..5 {
        let turn = recorder(&workspace, &format!("turn {index}"));
        turn.capture(&["f.txt".to_string()]).unwrap();
        ids.push(turn.finish().unwrap().id);
    }

    let store = CheckpointStore::open(&workspace).unwrap();
    let report = store
        .prune(RetentionLimits {
            max_checkpoints: 3,
            max_total_bytes: u64::MAX,
        })
        .unwrap();
    assert_eq!(report.removed, vec![ids[0].clone(), ids[1].clone()]);
    assert_eq!(report.kept, 3);

    // Byte budget: each checkpoint holds ~1 KB + manifest; a 2.5 KB cap
    // prunes down oldest-first but always keeps the newest.
    let report = store
        .prune(RetentionLimits {
            max_checkpoints: 50,
            max_total_bytes: 2_560,
        })
        .unwrap();
    assert!(!report.removed.is_empty());
    let remaining = store.list().unwrap();
    assert_eq!(
        remaining.first().map(|entry| entry.id.clone()),
        Some(ids[4].clone())
    );
    assert!(report.kept >= 1);
}

#[test]
fn list_returns_entries_newest_first_with_metadata() {
    let workspace = temp_workspace();
    fs::write(workspace.join("f.txt"), "v\n").unwrap();

    let first = CheckpointRecorder::new(&workspace, meta("first prompt", 3));
    first.capture(&["f.txt".to_string()]).unwrap();
    let first_id = first.finish().unwrap().id;

    let second = CheckpointRecorder::new(&workspace, meta("second prompt", 7));
    second.capture(&["f.txt".to_string()]).unwrap();
    let second_id = second.finish().unwrap().id;

    let entries = CheckpointStore::open(&workspace).unwrap().list().unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].id, second_id);
    assert_eq!(entries[1].id, first_id);
    assert_eq!(entries[0].prompt_excerpt, "second prompt");
    assert_eq!(entries[0].transcript_user_index, 7);
    assert_eq!(entries[0].session_id, "session-test.json");
    assert_eq!(entries[0].parent_id.as_deref(), Some(first_id.as_str()));
    assert!(entries[0].note.contains("shell-command"));
}

#[test]
fn retention_limits_env_overrides() {
    // Serialize env mutation within this test only; defaults are checked
    // via explicit construction elsewhere.
    unsafe {
        std::env::set_var("MEDUSA_CHECKPOINT_MAX", "7");
        std::env::set_var("MEDUSA_CHECKPOINT_MAX_MB", "3");
    }
    let limits = RetentionLimits::from_env();
    unsafe {
        std::env::remove_var("MEDUSA_CHECKPOINT_MAX");
        std::env::remove_var("MEDUSA_CHECKPOINT_MAX_MB");
    }

    assert_eq!(limits.max_checkpoints, 7);
    assert_eq!(limits.max_total_bytes, 3 * 1024 * 1024);
}

/// A hand-written (untrusted) manifest whose paths route through an
/// in-workspace symlink pointing OUTSIDE the workspace must not delete or
/// overwrite the host files it resolves to. Regression for finding [9].
#[cfg(unix)]
#[test]
fn restore_refuses_manifest_paths_escaping_through_symlink() {
    use std::os::unix::fs::symlink;

    let workspace = temp_workspace();
    let outside = temp_workspace();
    fs::write(outside.join("precious.txt"), "host secret\n").unwrap();
    fs::write(outside.join("config.txt"), "real config\n").unwrap();
    // `home` inside the workspace points at the outside directory.
    symlink(&outside, workspace.join("home")).unwrap();

    // Craft a manifest that would (a) delete host precious.txt via an
    // `absent` pre-image and (b) overwrite host config.txt via a `stored`
    // pre-image full of attacker bytes.
    let id = "cp-0000000000001-000001-0000";
    let cp_dir = checkpoints_dir(&workspace).join(id);
    fs::create_dir_all(cp_dir.join("files/home")).unwrap();
    fs::write(cp_dir.join("files/home/config.txt"), "ATTACKER\n").unwrap();
    let manifest = format!(
        r#"{{"id":"{id}","session_id":"s","created_at_ms":1,"prompt_excerpt":"x","transcript_user_index":0,"parent_id":null,"note":"n","files":[{{"path":"home/precious.txt","pre":"absent"}},{{"path":"home/config.txt","pre":"stored"}}]}}"#
    );
    fs::write(cp_dir.join("manifest.json"), manifest).unwrap();

    let store = CheckpointStore::open(&workspace).unwrap();
    let report = store.restore(id).unwrap();

    // Both entries refused; nothing restored or deleted.
    assert_eq!(report.refused.len(), 2);
    assert!(report.restored.is_empty());
    assert!(report.deleted.is_empty());
    // No safety checkpoint captured through the symlink either.
    assert!(report.safety_checkpoint.is_none());
    // The host files are untouched.
    assert!(
        outside.join("precious.txt").exists(),
        "restore must never delete a file outside the workspace"
    );
    assert_eq!(
        fs::read_to_string(outside.join("config.txt")).unwrap(),
        "real config\n",
        "restore must never overwrite a file outside the workspace"
    );
}

/// Absolute and `..` manifest paths are refused rather than resolved
/// against the host filesystem. Regression for finding [9].
#[test]
fn restore_refuses_absolute_and_parent_manifest_paths() {
    let workspace = temp_workspace();
    let id = "cp-0000000000002-000001-0000";
    let cp_dir = checkpoints_dir(&workspace).join(id);
    fs::create_dir_all(cp_dir.join("files")).unwrap();
    let manifest = format!(
        r#"{{"id":"{id}","session_id":"s","created_at_ms":1,"prompt_excerpt":"x","transcript_user_index":0,"parent_id":null,"note":"n","files":[{{"path":"/etc/hosts","pre":"absent"}},{{"path":"../outside.txt","pre":"absent"}},{{"path":".medusa/checkpoints/other/manifest.json","pre":"absent"}}]}}"#
    );
    fs::write(cp_dir.join("manifest.json"), manifest).unwrap();

    let report = CheckpointStore::open(&workspace)
        .unwrap()
        .restore(id)
        .unwrap();
    assert_eq!(report.refused.len(), 3);
    assert!(report.deleted.is_empty());
    assert!(report.restored.is_empty());
}

/// A symlink whose target is large slips past a `symlink_metadata().len()`
/// size check (the link's own len is tiny) and would be copied whole into
/// `.medusa`. Capture must instead skip it and never dereference it.
/// Regression for finding [13].
#[cfg(unix)]
#[test]
fn capture_skips_symlink_instead_of_copying_its_target() {
    use std::os::unix::fs::symlink;

    let workspace = temp_workspace();
    let target = workspace.join("real-data.bin");
    fs::write(&target, "x".repeat(4096)).unwrap();
    symlink(&target, workspace.join("link.bin")).unwrap();

    let turn = recorder(&workspace, "edit through symlink");
    turn.capture(&["link.bin".to_string()]).unwrap();
    let summary = turn.finish().unwrap();

    let entries = CheckpointStore::open(&workspace).unwrap().list().unwrap();
    let entry = entries.iter().find(|e| e.id == summary.id).unwrap();
    assert_eq!(entry.files.len(), 1);
    assert_eq!(entry.files[0].path, "link.bin");
    assert_eq!(entry.files[0].pre, PreImageKind::SkippedSymlink);
    // Crucially, the target content was NOT copied into the checkpoint.
    assert!(
        !checkpoints_dir(&workspace)
            .join(&summary.id)
            .join("files/link.bin")
            .exists(),
        "capture must not dereference the symlink and copy its target"
    );

    // Restore reports it not-rewound and leaves the link + target intact.
    let report = CheckpointStore::open(&workspace)
        .unwrap()
        .restore(&summary.id)
        .unwrap();
    assert_eq!(report.skipped, vec!["link.bin".to_string()]);
    assert!(report.restored.is_empty() && report.deleted.is_empty());
    assert!(
        fs::symlink_metadata(workspace.join("link.bin"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read_to_string(&target).unwrap(), "x".repeat(4096));
}

/// A file over the 50 MB cap is captured as `skipped_too_large` and restore
/// reports it as NOT rewound (leaving the post-turn content in place).
/// Flipping the restore arm to `restored` fails this test. Regression for
/// finding [25].
#[test]
fn too_large_file_is_skipped_on_capture_and_reported_not_rewound() {
    let workspace = temp_workspace();
    let over_cap = MAX_CAPTURED_FILE_BYTES as usize + 1;
    fs::write(workspace.join("big.bin"), vec![0u8; over_cap]).unwrap();

    let turn = recorder(&workspace, "touch big file");
    turn.capture(&["big.bin".to_string()]).unwrap();
    // Mutate after capture so a bug that "restored" it would be observable.
    fs::write(workspace.join("big.bin"), b"changed\n").unwrap();
    let summary = turn.finish().unwrap();

    let entries = CheckpointStore::open(&workspace).unwrap().list().unwrap();
    let entry = entries.iter().find(|e| e.id == summary.id).unwrap();
    assert_eq!(entry.files[0].pre, PreImageKind::SkippedTooLarge);
    assert!(
        !checkpoints_dir(&workspace)
            .join(&summary.id)
            .join("files/big.bin")
            .exists(),
        "a too-large file must not be copied into the checkpoint"
    );

    let report = CheckpointStore::open(&workspace)
        .unwrap()
        .restore(&summary.id)
        .unwrap();
    assert_eq!(report.skipped, vec!["big.bin".to_string()]);
    assert!(
        report.restored.is_empty(),
        "a skipped_too_large file must never be reported as restored"
    );
    assert_eq!(
        fs::read_to_string(workspace.join("big.bin")).unwrap(),
        "changed\n",
        "a too-large file keeps its post-turn content (it is not rewound)"
    );
}
