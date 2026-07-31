use super::*;

#[test]
fn file_read_reads_line_range() {
    let workspace = temp_workspace();
    fs::write(workspace.join("notes.txt"), "one\ntwo\nthree\nfour\n").unwrap();
    let runtime = ToolRuntime::new(&workspace).unwrap();

    let result = runtime
        .file_read(FileReadRequest {
            paths: vec![PathBuf::from("notes.txt")],
            start_line: Some(2),
            end_line: Some(3),
        })
        .unwrap();

    assert_eq!(result.files.len(), 1);
    assert_eq!(result.files[0].path, "notes.txt");
    assert_eq!(
        result.files[0].lines,
        vec![
            NumberedLine {
                number: 2,
                text: "two".to_string(),
            },
            NumberedLine {
                number: 3,
                text: "three".to_string(),
            },
        ]
    );
}

#[test]
fn file_search_finds_matches() {
    let workspace = temp_workspace();
    fs::create_dir_all(workspace.join("src")).unwrap();
    fs::write(
        workspace.join("src/main.rs"),
        "fn main() {}\nlet medusa = true;\n",
    )
    .unwrap();
    fs::write(workspace.join("README.md"), "Medusa\n").unwrap();
    let runtime = ToolRuntime::new(&workspace).unwrap();

    let result = runtime
        .file_search(FileSearchRequest {
            query: "medusa".to_string(),
            path: None,
            depth: Some(3),
            max_results: Some(10),
            case_sensitive: Some(false),
            include: None,
        })
        .unwrap();

    assert_eq!(result.matches.len(), 2);
    assert!(result.matches.iter().any(|hit| hit.path == "README.md"));
    assert!(result.matches.iter().any(|hit| hit.path == "src/main.rs"));
}

#[test]
fn file_search_supports_regex_queries() {
    let workspace = temp_workspace();
    fs::write(
        workspace.join("main.rs"),
        "fn alpha() {}\nfn beta_helper() {}\nlet x = 1;\n",
    )
    .unwrap();
    let runtime = ToolRuntime::new(&workspace).unwrap();

    let result = runtime
        .file_search(FileSearchRequest {
            query: r"fn \w+\(\)".to_string(),
            path: None,
            depth: Some(2),
            max_results: Some(10),
            case_sensitive: Some(true),
            include: None,
        })
        .unwrap();

    assert!(result.regex);
    assert_eq!(result.matches.len(), 2);
}

#[test]
fn file_search_falls_back_to_literal_on_invalid_regex() {
    let workspace = temp_workspace();
    fs::write(workspace.join("notes.txt"), "weird (unbalanced text\n").unwrap();
    let runtime = ToolRuntime::new(&workspace).unwrap();

    let result = runtime
        .file_search(FileSearchRequest {
            query: "(unbalanced".to_string(),
            path: None,
            depth: Some(2),
            max_results: Some(10),
            case_sensitive: Some(true),
            include: None,
        })
        .unwrap();

    assert!(!result.regex);
    assert_eq!(result.matches.len(), 1);
}

#[test]
fn file_search_include_filters_by_glob() {
    let workspace = temp_workspace();
    fs::create_dir_all(workspace.join("src")).unwrap();
    fs::write(workspace.join("src/main.rs"), "medusa\n").unwrap();
    fs::write(workspace.join("README.md"), "medusa\n").unwrap();
    let runtime = ToolRuntime::new(&workspace).unwrap();

    let result = runtime
        .file_search(FileSearchRequest {
            query: "medusa".to_string(),
            path: None,
            depth: Some(3),
            max_results: Some(10),
            case_sensitive: Some(false),
            include: Some("*.rs".to_string()),
        })
        .unwrap();

    assert_eq!(result.matches.len(), 1);
    assert_eq!(result.matches[0].path, "src/main.rs");
}

#[test]
fn file_glob_matches_and_skips_noise_dirs() {
    let workspace = temp_workspace();
    fs::create_dir_all(workspace.join("src/model")).unwrap();
    fs::create_dir_all(workspace.join("target/debug")).unwrap();
    fs::write(workspace.join("main.rs"), "").unwrap();
    fs::write(workspace.join("src/lib.rs"), "").unwrap();
    fs::write(workspace.join("src/model/wire.rs"), "").unwrap();
    fs::write(workspace.join("src/notes.md"), "").unwrap();
    fs::write(workspace.join("target/debug/gen.rs"), "").unwrap();
    let runtime = ToolRuntime::new(&workspace).unwrap();

    let result = runtime
        .file_glob(FileGlobRequest {
            pattern: "*.rs".to_string(),
            path: None,
            max_results: Some(50),
        })
        .unwrap();

    assert_eq!(result.paths.len(), 3);
    assert!(result.paths.contains(&"main.rs".to_string()));
    assert!(result.paths.contains(&"src/lib.rs".to_string()));
    assert!(result.paths.contains(&"src/model/wire.rs".to_string()));

    let scoped = runtime
        .file_glob(FileGlobRequest {
            pattern: "src/**/*.rs".to_string(),
            path: None,
            max_results: Some(50),
        })
        .unwrap();

    assert_eq!(scoped.paths.len(), 2);
}

#[test]
fn fs_list_skips_noise_dirs() {
    let workspace = temp_workspace();
    fs::create_dir_all(workspace.join("src")).unwrap();
    fs::create_dir_all(workspace.join("target/debug")).unwrap();
    fs::write(workspace.join("src/lib.rs"), "").unwrap();
    fs::write(workspace.join("target/debug/noise"), "").unwrap();
    let runtime = ToolRuntime::new(&workspace).unwrap();

    let result = runtime
        .fs_list(FsListRequest {
            path: None,
            depth: Some(3),
            max_entries: Some(50),
        })
        .unwrap();

    assert!(
        result
            .entries
            .iter()
            .any(|entry| entry.path == "src/lib.rs")
    );
    assert!(
        !result
            .entries
            .iter()
            .any(|entry| entry.path.contains("target"))
    );
}

#[test]
fn explore_batch_runs_read_only_probes_in_order() {
    let workspace = temp_workspace();
    fs::create_dir_all(workspace.join("src")).unwrap();
    fs::write(workspace.join("src/lib.rs"), "pub fn medusa() {}\n").unwrap();
    fs::write(workspace.join("README.md"), "Medusa harness\n").unwrap();
    let runtime = ToolRuntime::new(&workspace).unwrap();

    let result = runtime
        .explore_batch(ExploreBatchRequest {
            goal: "understand repo".to_string(),
            probes: vec![
                ExploreProbe {
                    kind: ExploreProbeKind::List,
                    query: None,
                    path: None,
                    paths: Vec::new(),
                    command: None,
                    cwd: None,
                    start_line: None,
                    end_line: None,
                    depth: Some(2),
                    max_results: None,
                    max_entries: Some(20),
                    case_sensitive: None,
                },
                ExploreProbe {
                    kind: ExploreProbeKind::Search,
                    query: Some("medusa".to_string()),
                    path: None,
                    paths: Vec::new(),
                    command: None,
                    cwd: None,
                    start_line: None,
                    end_line: None,
                    depth: Some(3),
                    max_results: Some(10),
                    max_entries: None,
                    case_sensitive: Some(false),
                },
                ExploreProbe {
                    kind: ExploreProbeKind::Read,
                    query: None,
                    path: None,
                    paths: vec![PathBuf::from("README.md")],
                    command: None,
                    cwd: None,
                    start_line: Some(1),
                    end_line: Some(1),
                    depth: None,
                    max_results: None,
                    max_entries: None,
                    case_sensitive: None,
                },
            ],
        })
        .unwrap();

    assert_eq!(result.failed, 0);
    assert_eq!(result.probes.len(), 3);
    assert_eq!(result.probes[0].kind, "list");
    assert_eq!(result.probes[1].kind, "search");
    assert_eq!(result.probes[2].kind, "read");
    assert!(result.probes[0].output.contains("src/lib.rs"));
    assert!(result.probes[1].output.contains("matches: 2"));
    assert!(result.probes[2].output.contains("Medusa harness"));
}

#[test]
fn explore_batch_rejects_mutating_terminal_probe() {
    let workspace = temp_workspace();
    let runtime = ToolRuntime::new(&workspace).unwrap();

    let result = runtime
        .explore_batch(ExploreBatchRequest {
            goal: "bad probe".to_string(),
            probes: vec![ExploreProbe {
                kind: ExploreProbeKind::Terminal,
                query: None,
                path: None,
                paths: Vec::new(),
                command: Some("rm -rf target".to_string()),
                cwd: None,
                start_line: None,
                end_line: None,
                depth: None,
                max_results: None,
                max_entries: None,
                case_sensitive: None,
            }],
        })
        .unwrap();

    assert_eq!(result.failed, 1);
    assert!(result.probes[0].failed);
    assert!(result.probes[0].output.contains("read-only"));
}
