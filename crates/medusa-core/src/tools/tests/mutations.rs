use super::*;

#[test]
fn file_patch_applies_unified_diff() {
    let workspace = temp_workspace();
    fs::write(workspace.join("hello.txt"), "old\n").unwrap();
    let runtime = ToolRuntime::new(&workspace).unwrap();

    let diff = r#"diff --git a/hello.txt b/hello.txt
--- a/hello.txt
+++ b/hello.txt
@@ -1 +1 @@
-old
+new
"#;

    let result = runtime.file_patch(FilePatchRequest::new(diff)).unwrap();

    assert_eq!(result.changed_files, vec!["hello.txt"]);
    assert_eq!(
        fs::read_to_string(workspace.join("hello.txt")).unwrap(),
        "new\n"
    );
}

#[test]
fn file_patch_accepts_fenced_diff() {
    let workspace = temp_workspace();
    fs::write(workspace.join("hello.txt"), "old\n").unwrap();
    let runtime = ToolRuntime::new(&workspace).unwrap();

    let diff = r#"```diff
diff --git a/hello.txt b/hello.txt
--- a/hello.txt
+++ b/hello.txt
@@ -1 +1 @@
-old
+new
```
"#;

    let result = runtime.file_patch(FilePatchRequest::new(diff)).unwrap();

    assert_eq!(result.changed_files, vec!["hello.txt"]);
    assert_eq!(
        fs::read_to_string(workspace.join("hello.txt")).unwrap(),
        "new\n"
    );
}

#[test]
fn file_patch_accepts_codex_update_patch() {
    let workspace = temp_workspace();
    fs::write(workspace.join("hello.txt"), "alpha\nold\nomega\n").unwrap();
    let runtime = ToolRuntime::new(&workspace).unwrap();

    let diff = r#"*** Begin Patch
*** Update File: hello.txt
@@
 alpha
-old
+new
 omega
*** End Patch
"#;

    let result = runtime.file_patch(FilePatchRequest::new(diff)).unwrap();

    assert_eq!(result.changed_files, vec!["hello.txt"]);
    assert_eq!(
        fs::read_to_string(workspace.join("hello.txt")).unwrap(),
        "alpha\nnew\nomega\n"
    );
}

#[test]
fn file_patch_accepts_codex_add_delete_and_move() {
    let workspace = temp_workspace();
    fs::write(workspace.join("delete-me.txt"), "bye\n").unwrap();
    fs::write(workspace.join("move-me.txt"), "move\n").unwrap();
    let runtime = ToolRuntime::new(&workspace).unwrap();

    let diff = r#"*** Begin Patch
*** Add File: src/new.txt
+hello
*** Delete File: delete-me.txt
*** Update File: move-me.txt
*** Move to: moved.txt
*** End Patch
"#;

    let result = runtime.file_patch(FilePatchRequest::new(diff)).unwrap();

    assert_eq!(
        result.changed_files,
        vec![
            "delete-me.txt".to_string(),
            "move-me.txt".to_string(),
            "moved.txt".to_string(),
            "src/new.txt".to_string(),
        ]
    );
    assert_eq!(
        fs::read_to_string(workspace.join("src/new.txt")).unwrap(),
        "hello\n"
    );
    assert!(!workspace.join("delete-me.txt").exists());
    assert!(!workspace.join("move-me.txt").exists());
    assert_eq!(
        fs::read_to_string(workspace.join("moved.txt")).unwrap(),
        "move\n"
    );
}

#[test]
fn codex_patch_rolls_back_earlier_files_when_a_later_hunk_fails() {
    let workspace = temp_workspace();
    fs::write(workspace.join("first.txt"), "old first\n").unwrap();
    fs::write(workspace.join("second.txt"), "old second\n").unwrap();
    let runtime = ToolRuntime::new(&workspace).unwrap();

    let diff = r#"*** Begin Patch
*** Update File: first.txt
@@
-old first
+new first
*** Update File: second.txt
@@
-content that is not present
+new second
*** End Patch
"#;

    let error = runtime.file_patch(FilePatchRequest::new(diff)).unwrap_err();

    assert!(error.to_string().contains("rolled back"), "{error:#}");
    assert_eq!(
        fs::read_to_string(workspace.join("first.txt")).unwrap(),
        "old first\n"
    );
    assert_eq!(
        fs::read_to_string(workspace.join("second.txt")).unwrap(),
        "old second\n"
    );
}

#[test]
fn file_edit_replaces_exact_string_once() {
    let workspace = temp_workspace();
    fs::write(workspace.join("hello.txt"), "alpha\nold\nomega\n").unwrap();
    let runtime = ToolRuntime::new(&workspace).unwrap();

    let result = runtime
        .file_edit(FileEditRequest::new("hello.txt", "old\n", "new\n"))
        .unwrap();

    assert_eq!(result.path, "hello.txt");
    assert_eq!(result.replacements, 1);
    assert_eq!(
        fs::read_to_string(workspace.join("hello.txt")).unwrap(),
        "alpha\nnew\nomega\n"
    );
}

#[test]
fn file_edit_requires_replace_all_for_multiple_matches() {
    let workspace = temp_workspace();
    fs::write(workspace.join("hello.txt"), "old\nold\n").unwrap();
    let runtime = ToolRuntime::new(&workspace).unwrap();

    let error = runtime
        .file_edit(FileEditRequest::new("hello.txt", "old", "new"))
        .unwrap_err();

    assert!(error.to_string().contains("matched 2 times"), "{error:?}");
}

#[test]
fn file_edit_can_create_new_file_with_empty_old_string() {
    let workspace = temp_workspace();
    let runtime = ToolRuntime::new(&workspace).unwrap();

    let result = runtime
        .file_edit(FileEditRequest::new("src/new.txt", "", "hello\n"))
        .unwrap();

    assert_eq!(result.path, "src/new.txt");
    assert_eq!(
        fs::read_to_string(workspace.join("src/new.txt")).unwrap(),
        "hello\n"
    );
}

#[test]
fn file_patch_rejects_parent_paths() {
    let workspace = temp_workspace();
    let runtime = ToolRuntime::new(&workspace).unwrap();

    let diff = r#"diff --git a/../outside.txt b/../outside.txt
--- a/../outside.txt
+++ b/../outside.txt
@@ -1 +1 @@
-old
+new
"#;

    let error = runtime.file_patch(FilePatchRequest::new(diff)).unwrap_err();

    assert!(error.to_string().contains("escapes workspace"), "{error:?}");
}

#[test]
fn terminal_exec_obeys_permission_policy() {
    let workspace = temp_workspace();
    write_permissions(&workspace, r#"{"terminal":{"deny_contains":["nope"]}}"#);
    let runtime = ToolRuntime::new(&workspace).unwrap();

    let error = runtime
        .terminal_exec(TerminalExecRequest::new("printf nope"))
        .unwrap_err();

    assert!(error.to_string().contains("terminal.exec denied"));
}

#[test]
fn file_patch_obeys_permission_policy() {
    let workspace = temp_workspace();
    fs::write(workspace.join("README.md"), "old\n").unwrap();
    write_permissions(&workspace, r#"{"patch":{"allow_prefixes":["crates/"]}}"#);
    let runtime = ToolRuntime::new(&workspace).unwrap();

    let diff = r#"diff --git a/README.md b/README.md
--- a/README.md
+++ b/README.md
@@ -1 +1 @@
-old
+new
"#;

    let error = runtime.file_patch(FilePatchRequest::new(diff)).unwrap_err();

    assert!(error.to_string().contains("file.patch denied"));
    assert_eq!(
        fs::read_to_string(workspace.join("README.md")).unwrap(),
        "old\n"
    );
}

#[test]
fn file_patch_checks_paths_relative_to_workspace_not_cwd() {
    let workspace = temp_workspace();
    fs::create_dir_all(workspace.join(".medusa/sessions")).unwrap();
    fs::write(workspace.join(".medusa/sessions/session.json"), "old\n").unwrap();
    write_permissions(
        &workspace,
        r#"{"patch":{"deny_prefixes":[".medusa/sessions/"]}}"#,
    );
    let runtime = ToolRuntime::new(&workspace).unwrap();

    let diff = r#"diff --git a/sessions/session.json b/sessions/session.json
--- a/sessions/session.json
+++ b/sessions/session.json
@@ -1 +1 @@
-old
+new
"#;

    let error = runtime
        .file_patch(FilePatchRequest {
            diff: diff.to_string(),
            cwd: Some(PathBuf::from(".medusa")),
            description: None,
        })
        .unwrap_err();

    assert!(error.to_string().contains("file.patch denied"), "{error:?}");
    assert_eq!(
        fs::read_to_string(workspace.join(".medusa/sessions/session.json")).unwrap(),
        "old\n"
    );
}
