use std::{
    collections::BTreeSet,
    ffi::OsStr,
    fs,
    hash::{Hash, Hasher},
    io::Write,
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc::{self, Sender},
    thread,
    time::Instant,
};

use color_eyre::eyre::{Result, WrapErr, bail};

use crate::hooks::HookRuntime;
use crate::permissions::PermissionPolicy;
use crate::skills::SkillRegistry;

#[derive(Debug, Clone)]
pub struct ToolRuntime {
    workspace: PathBuf,
    hooks: HookRuntime,
    permissions: PermissionPolicy,
    skills: SkillRegistry,
    background_events: Option<Sender<BackgroundJobEvent>>,
}

impl ToolRuntime {
    pub fn new(workspace: impl Into<PathBuf>) -> Result<Self> {
        let workspace = workspace.into();
        let workspace = workspace
            .canonicalize()
            .wrap_err_with(|| format!("workspace does not exist: {}", workspace.display()))?;
        let hooks = HookRuntime::load(&workspace)?;
        let permissions = PermissionPolicy::load(&workspace)?;
        let skills = SkillRegistry::load(&workspace)?;

        Ok(Self {
            workspace,
            hooks,
            permissions,
            skills,
            background_events: None,
        })
    }

    pub fn with_background_events(mut self, sender: Sender<BackgroundJobEvent>) -> Self {
        self.background_events = Some(sender);
        self
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    pub fn hooks(&self) -> &HookRuntime {
        &self.hooks
    }

    pub fn skills(&self) -> &SkillRegistry {
        &self.skills
    }

    pub fn terminal_exec(&self, request: TerminalExecRequest) -> Result<TerminalExecResult> {
        self.permissions.check_terminal_command(&request.command)?;
        let cwd = self.resolve_workspace_path(request.cwd.as_deref())?;
        let shell = std::env::var_os("SHELL").unwrap_or_else(|| OsStr::new("sh").to_os_string());

        if request.background {
            let mut child = Command::new(shell)
                .arg("-lc")
                .arg(&request.command)
                .current_dir(&cwd)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .wrap_err_with(|| format!("failed to start command: {}", request.command))?;

            let pid = child.id();
            let id = background_job_id(pid, &request.command, &cwd);
            let command = request.command.clone();
            let event_cwd = cwd.clone();
            if let Some(sender) = self.background_events.clone() {
                let _ = sender.send(BackgroundJobEvent::Started {
                    id: id.clone(),
                    pid,
                    command: command.clone(),
                    cwd: event_cwd.clone(),
                });
                let finish_id = id.clone();
                let fail_id = id.clone();
                thread::spawn(move || {
                    let event = match child.wait_with_output() {
                        Ok(output) => BackgroundJobEvent::Finished {
                            id: finish_id,
                            pid,
                            command,
                            cwd: event_cwd,
                            code: output.status.code(),
                            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                        },
                        Err(error) => BackgroundJobEvent::Failed {
                            id: fail_id,
                            pid,
                            command,
                            cwd: event_cwd,
                            error: error.to_string(),
                        },
                    };
                    let _ = sender.send(event);
                });
            } else {
                thread::spawn(move || {
                    let _ = child.wait();
                });
            }

            return Ok(TerminalExecResult {
                command: request.command,
                cwd,
                code: Some(0),
                stdout: String::new(),
                stderr: String::new(),
                background: true,
                pid: Some(pid),
                job_id: Some(id),
            });
        }

        let output = Command::new(shell)
            .arg("-lc")
            .arg(&request.command)
            .current_dir(&cwd)
            .output()
            .wrap_err_with(|| format!("failed to run command: {}", request.command))?;

        Ok(TerminalExecResult {
            command: request.command,
            cwd,
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            background: false,
            pid: None,
            job_id: None,
        })
    }

    pub fn file_read(&self, request: FileReadRequest) -> Result<FileReadResult> {
        if request.paths.is_empty() {
            bail!("file_read.paths cannot be empty");
        }

        let start_line = request.start_line.unwrap_or(1).max(1);
        let end_line = request.end_line.unwrap_or(start_line + 240).max(start_line);
        let mut files = Vec::new();

        for path in request.paths {
            let resolved = self.resolve_workspace_path(Some(&path))?;
            if !resolved.is_file() {
                bail!("file_read path is not a file: {}", path.display());
            }

            let content = fs::read_to_string(&resolved)
                .wrap_err_with(|| format!("failed to read {}", resolved.display()))?;
            let lines = content.lines().collect::<Vec<_>>();
            let total_lines = lines.len();
            let start_index = start_line.saturating_sub(1).min(total_lines);
            let end_index = end_line.min(total_lines);
            let mut selected = lines[start_index..end_index]
                .iter()
                .enumerate()
                .map(|(offset, line)| NumberedLine {
                    number: start_line + offset,
                    text: (*line).to_string(),
                })
                .collect::<Vec<_>>();
            let truncated = selected.len() > 260;
            selected.truncate(260);

            files.push(ReadFile {
                path: self.workspace_relative(&resolved),
                start_line,
                end_line: if selected.is_empty() {
                    start_line
                } else {
                    selected.last().map_or(start_line, |line| line.number)
                },
                total_lines,
                truncated,
                lines: selected,
            });
        }

        Ok(FileReadResult { files })
    }

    pub fn file_search(&self, request: FileSearchRequest) -> Result<FileSearchResult> {
        let query = request.query.trim();
        if query.is_empty() {
            bail!("file_search.query cannot be empty");
        }

        let root = self.resolve_workspace_path(request.path.as_deref())?;
        let max_results = request.max_results.unwrap_or(80).clamp(1, 500);
        let case_sensitive = request.case_sensitive.unwrap_or(true);
        let needle = if case_sensitive {
            query.to_string()
        } else {
            query.to_ascii_lowercase()
        };
        let mut matches = Vec::new();
        let mut searched_files = 0usize;

        for file in self.walk_files(&root, request.depth.unwrap_or(8).clamp(0, 16))? {
            if matches.len() >= max_results {
                break;
            }
            if file.metadata().map(|meta| meta.len()).unwrap_or(0) > 2_000_000 {
                continue;
            }
            let Ok(content) = fs::read_to_string(&file) else {
                continue;
            };
            searched_files += 1;
            for (line_index, line) in content.lines().enumerate() {
                let haystack = if case_sensitive {
                    line.to_string()
                } else {
                    line.to_ascii_lowercase()
                };
                if haystack.contains(&needle) {
                    matches.push(SearchMatch {
                        path: self.workspace_relative(&file),
                        line: line_index + 1,
                        text: line.trim_end().chars().take(240).collect(),
                    });
                    if matches.len() >= max_results {
                        break;
                    }
                }
            }
        }

        let truncated = matches.len() >= max_results;
        Ok(FileSearchResult {
            query: query.to_string(),
            matches,
            searched_files,
            truncated,
        })
    }

    pub fn fs_list(&self, request: FsListRequest) -> Result<FsListResult> {
        let root = self.resolve_workspace_path(request.path.as_deref())?;
        let max_depth = request.depth.unwrap_or(2).clamp(0, 8);
        let max_entries = request.max_entries.unwrap_or(120).clamp(1, 500);
        let mut entries = Vec::new();
        let mut truncated = false;

        self.collect_list_entries(
            &root,
            0,
            max_depth,
            max_entries,
            &mut entries,
            &mut truncated,
        )?;

        Ok(FsListResult {
            root: self.workspace_relative(&root),
            entries,
            truncated,
        })
    }

    pub fn file_patch(&self, request: FilePatchRequest) -> Result<FilePatchResult> {
        let cwd = self.resolve_workspace_path(request.cwd.as_deref())?;
        let diff = normalize_patch(&request.diff);
        let diff = diff.as_str();

        if diff.trim().is_empty() {
            bail!("patch is empty");
        }

        let changed_files = extract_patch_paths(diff)?;
        if changed_files.is_empty() {
            bail!("patch does not contain any file paths");
        }

        let workspace_changed_files = self.workspace_relative_patch_paths(&cwd, &changed_files)?;
        self.permissions
            .check_patch_paths(&workspace_changed_files)?;

        if is_codex_patch(diff) {
            apply_codex_patch(&cwd, diff)?;
            return Ok(FilePatchResult {
                changed_files: workspace_changed_files,
            });
        }

        let mut recount = false;
        if let Err(error) = run_git_apply(&cwd, diff, true, false) {
            let first_error = error.to_string();
            recount = true;
            run_git_apply(&cwd, diff, true, true)
                .wrap_err_with(|| format!("{first_error}; retry with --recount also failed"))?;
        }
        run_git_apply(&cwd, diff, false, recount)?;

        Ok(FilePatchResult {
            changed_files: workspace_changed_files,
        })
    }

    pub fn file_edit(&self, request: FileEditRequest) -> Result<FileEditResult> {
        let path = request.path.to_string_lossy().to_string();
        validate_relative_path(&path)?;
        self.permissions
            .check_patch_paths(std::slice::from_ref(&path))?;

        if request.old_string == request.new_string {
            bail!("old_string and new_string must differ");
        }

        let candidate = self.workspace.join(&request.path);
        if !candidate.exists() {
            if !request.old_string.is_empty() {
                bail!("file_edit target does not exist: {}", path);
            }
            if let Some(parent) = candidate.parent() {
                let existing_parent = parent
                    .ancestors()
                    .find(|ancestor| ancestor.exists())
                    .unwrap_or(&self.workspace);
                let canonical_parent = existing_parent
                    .canonicalize()
                    .wrap_err_with(|| format!("failed to resolve {}", existing_parent.display()))?;
                if !canonical_parent.starts_with(&self.workspace) {
                    bail!("path escapes workspace: {}", candidate.display());
                }
                fs::create_dir_all(parent)
                    .wrap_err_with(|| format!("failed to create {}", parent.display()))?;
            }
            fs::write(&candidate, request.new_string)
                .wrap_err_with(|| format!("failed to write {}", candidate.display()))?;
            return Ok(FileEditResult {
                path,
                replacements: 1,
            });
        }

        if request.old_string.is_empty() {
            bail!("old_string cannot be empty for existing files");
        }

        let resolved = self.resolve_workspace_path(Some(&request.path))?;
        if !resolved.is_file() {
            bail!("file_edit path is not a file: {}", path);
        }

        let content = fs::read_to_string(&resolved)
            .wrap_err_with(|| format!("failed to read {}", resolved.display()))?;
        let matches = content
            .match_indices(&request.old_string)
            .collect::<Vec<_>>();
        if matches.is_empty() {
            let normalized_old = request.old_string.replace("\r\n", "\n");
            let normalized_content = content.replace("\r\n", "\n");
            if normalized_old != request.old_string
                && normalized_content.matches(&normalized_old).count() > 0
            {
                bail!(
                    "old_string was not found exactly; it appears to match only after line-ending normalization. Re-read the file and retry with exact text."
                );
            }
            bail!("old_string was not found exactly once in {}", path);
        }
        if matches.len() > 1 && !request.replace_all {
            bail!(
                "old_string matched {} times in {}; provide more context or set replace_all=true",
                matches.len(),
                path
            );
        }

        let new_content = if request.replace_all {
            content.replace(&request.old_string, &request.new_string)
        } else {
            content.replacen(&request.old_string, &request.new_string, 1)
        };
        fs::write(&resolved, new_content)
            .wrap_err_with(|| format!("failed to write {}", resolved.display()))?;

        Ok(FileEditResult {
            path,
            replacements: if request.replace_all {
                matches.len()
            } else {
                1
            },
        })
    }

    pub fn task_update(&self, request: TaskUpdateRequest) -> Result<TaskUpdateResult> {
        let status = request.status.trim();
        if status.is_empty() {
            bail!("status cannot be empty");
        }

        Ok(TaskUpdateResult {
            status: status.chars().take(160).collect(),
        })
    }

    pub fn plan_update(&self, request: PlanUpdateRequest) -> Result<PlanUpdateResult> {
        if request.items.is_empty() {
            bail!("plan items cannot be empty");
        }

        let mut active_count = 0usize;
        let mut items = Vec::new();
        for item in request.items.into_iter().take(24) {
            let text = item.text.trim();
            if text.is_empty() {
                continue;
            }
            let status = normalize_plan_status(&item.status)
                .ok_or_else(|| color_eyre::eyre::eyre!("unknown plan status: {}", item.status))?;
            if status == "active" {
                active_count += 1;
            }
            let evidence = item
                .evidence
                .into_iter()
                .map(|value| value.trim().chars().take(180).collect::<String>())
                .filter(|value| !value.is_empty())
                .take(6)
                .collect::<Vec<_>>();
            items.push(PlanUpdateItem {
                text: text.chars().take(220).collect(),
                status: status.to_string(),
                evidence,
            });
        }

        if items.is_empty() {
            bail!("plan items cannot all be empty");
        }
        if active_count > 1 {
            bail!("at most one plan item can be active");
        }

        Ok(PlanUpdateResult {
            summary: request
                .summary
                .unwrap_or_default()
                .trim()
                .chars()
                .take(180)
                .collect(),
            items,
        })
    }

    pub fn question(&self, request: QuestionRequest) -> Result<QuestionResult> {
        let question = request.question.trim();
        if question.is_empty() {
            bail!("question cannot be empty");
        }

        Ok(QuestionResult {
            question: question.chars().take(600).collect(),
        })
    }

    pub fn decision_request(&self, request: DecisionRequest) -> Result<DecisionResult> {
        if request.questions.is_empty() {
            bail!("decision_request.questions cannot be empty");
        }

        let mut seen_ids = BTreeSet::new();
        let mut questions = Vec::new();
        for (index, question) in request.questions.into_iter().take(8).enumerate() {
            let prompt = question.prompt.trim();
            if prompt.is_empty() {
                continue;
            }

            let kind = normalize_decision_kind(&question.kind);
            let options = question
                .options
                .into_iter()
                .map(|option| option.trim().chars().take(120).collect::<String>())
                .filter(|option| !option.is_empty())
                .take(8)
                .collect::<Vec<_>>();
            if kind == "choice" && options.is_empty() {
                bail!(
                    "decision_request.questions[{index}].options is required for choice questions"
                );
            }

            let base_id = sanitize_decision_id(&question.id)
                .filter(|id| !id.is_empty())
                .unwrap_or_else(|| format!("q{}", index + 1));
            let mut id = base_id.clone();
            let mut suffix = 2usize;
            while !seen_ids.insert(id.clone()) {
                id = format!("{base_id}_{suffix}");
                suffix += 1;
            }

            let recommended = question
                .recommended
                .map(|value| value.trim().chars().take(120).collect::<String>())
                .filter(|value| !value.is_empty());

            questions.push(DecisionQuestion {
                id,
                prompt: prompt.chars().take(280).collect(),
                kind: kind.to_string(),
                options,
                recommended,
                required: question.required,
            });
        }

        if questions.is_empty() {
            bail!("decision_request.questions cannot all be empty");
        }

        let assumptions = request
            .assumptions
            .into_iter()
            .map(|assumption| assumption.trim().chars().take(220).collect::<String>())
            .filter(|assumption| !assumption.is_empty())
            .take(6)
            .collect::<Vec<_>>();

        Ok(DecisionResult {
            title: request
                .title
                .unwrap_or_else(|| "Planning decision".to_string())
                .trim()
                .chars()
                .take(120)
                .collect(),
            reason: request
                .reason
                .unwrap_or_default()
                .trim()
                .chars()
                .take(280)
                .collect(),
            questions,
            assumptions,
        })
    }

    pub fn explore_batch(&self, request: ExploreBatchRequest) -> Result<ExploreBatchResult> {
        if request.probes.is_empty() {
            bail!("explore_batch.probes cannot be empty");
        }

        let goal = request.goal.trim().chars().take(240).collect::<String>();
        let probes = request.probes.into_iter().take(12).collect::<Vec<_>>();
        let total = probes.len();
        let batch_started = Instant::now();
        let (sender, receiver) = mpsc::channel();

        for (index, probe) in probes.into_iter().enumerate() {
            let sender = sender.clone();
            let tools = self.clone();
            thread::spawn(move || {
                let result = run_explore_probe(&tools, index, probe);
                let _ = sender.send((index, result));
            });
        }
        drop(sender);

        let mut results = vec![None; total];
        for (index, result) in receiver {
            if let Some(slot) = results.get_mut(index) {
                *slot = Some(result);
            }
        }

        let probes = results
            .into_iter()
            .enumerate()
            .map(|(index, result)| {
                result.unwrap_or_else(|| ExploreProbeResult {
                    index,
                    kind: "unknown".to_string(),
                    label: format!("probe {}", index + 1),
                    failed: true,
                    output: "probe worker ended without returning a result".to_string(),
                    elapsed_ms: 0,
                })
            })
            .collect::<Vec<_>>();
        let failed = probes.iter().filter(|probe| probe.failed).count();

        Ok(ExploreBatchResult {
            goal,
            probes,
            failed,
            elapsed_ms: batch_started.elapsed().as_millis(),
        })
    }

    pub fn read_patch_file(&self, path: &str) -> Result<String> {
        let path = self.resolve_workspace_path(Some(Path::new(path)))?;
        fs::read_to_string(&path)
            .wrap_err_with(|| format!("failed to read patch file: {}", path.display()))
    }

    fn resolve_workspace_path(&self, path: Option<&Path>) -> Result<PathBuf> {
        let Some(path) = path else {
            return Ok(self.workspace.clone());
        };

        let candidate = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.workspace.join(path)
        };

        let canonical = candidate
            .canonicalize()
            .wrap_err_with(|| format!("path does not exist: {}", candidate.display()))?;

        if !canonical.starts_with(&self.workspace) {
            bail!(
                "path escapes workspace: {} is outside {}",
                canonical.display(),
                self.workspace.display()
            );
        }

        Ok(canonical)
    }

    fn workspace_relative(&self, path: &Path) -> String {
        path.strip_prefix(&self.workspace)
            .unwrap_or(path)
            .to_string_lossy()
            .trim_start_matches('/')
            .to_string()
            .if_empty(".")
    }

    fn workspace_relative_patch_paths(&self, cwd: &Path, paths: &[String]) -> Result<Vec<String>> {
        let cwd_relative = cwd.strip_prefix(&self.workspace).wrap_err_with(|| {
            format!(
                "patch cwd {} is outside workspace {}",
                cwd.display(),
                self.workspace.display()
            )
        })?;
        let mut normalized = BTreeSet::new();

        for path in paths {
            validate_relative_path(path)?;
            let workspace_path = cwd_relative.join(path);
            normalized.insert(normalize_workspace_relative_path(&workspace_path)?);
        }

        Ok(normalized.into_iter().collect())
    }

    fn walk_files(&self, root: &Path, max_depth: usize) -> Result<Vec<PathBuf>> {
        let mut files = Vec::new();
        self.collect_files(root, 0, max_depth, &mut files)?;
        files.sort();
        Ok(files)
    }

    fn collect_files(
        &self,
        path: &Path,
        depth: usize,
        max_depth: usize,
        files: &mut Vec<PathBuf>,
    ) -> Result<()> {
        if path.is_file() {
            files.push(path.to_path_buf());
            return Ok(());
        }
        if depth > max_depth || !path.is_dir() {
            return Ok(());
        }

        for entry in sorted_read_dir(path)? {
            let entry_path = entry.path();
            if entry_path.is_dir() {
                if should_skip_dir(&entry_path) {
                    continue;
                }
                self.collect_files(&entry_path, depth + 1, max_depth, files)?;
            } else if entry_path.is_file() {
                files.push(entry_path);
            }
        }

        Ok(())
    }

    fn collect_list_entries(
        &self,
        path: &Path,
        depth: usize,
        max_depth: usize,
        max_entries: usize,
        entries: &mut Vec<FsEntry>,
        truncated: &mut bool,
    ) -> Result<()> {
        if entries.len() >= max_entries {
            *truncated = true;
            return Ok(());
        }

        if depth > 0 && path.is_dir() && should_skip_dir(path) {
            return Ok(());
        }

        if path != self.workspace || depth > 0 {
            entries.push(FsEntry {
                path: self.workspace_relative(path),
                kind: if path.is_dir() { "dir" } else { "file" }.to_string(),
                depth,
            });
        }

        if depth >= max_depth || !path.is_dir() {
            return Ok(());
        }

        for entry in sorted_read_dir(path)? {
            if entries.len() >= max_entries {
                *truncated = true;
                break;
            }
            self.collect_list_entries(
                &entry.path(),
                depth + 1,
                max_depth,
                max_entries,
                entries,
                truncated,
            )?;
        }

        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalExecRequest {
    pub command: String,
    pub cwd: Option<PathBuf>,
    pub background: bool,
}

impl TerminalExecRequest {
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            cwd: None,
            background: false,
        }
    }

    pub fn background(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            cwd: None,
            background: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalExecResult {
    pub command: String,
    pub cwd: PathBuf,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub background: bool,
    pub pid: Option<u32>,
    pub job_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackgroundJobEvent {
    Started {
        id: String,
        pid: u32,
        command: String,
        cwd: PathBuf,
    },
    Finished {
        id: String,
        pid: u32,
        command: String,
        cwd: PathBuf,
        code: Option<i32>,
        stdout: String,
        stderr: String,
    },
    Failed {
        id: String,
        pid: u32,
        command: String,
        cwd: PathBuf,
        error: String,
    },
}

fn background_job_id(pid: u32, command: &str, cwd: &Path) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    pid.hash(&mut hasher);
    command.hash(&mut hasher);
    cwd.hash(&mut hasher);
    format!("bg-{pid}-{:x}", hasher.finish() & 0xffff)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileReadRequest {
    pub paths: Vec<PathBuf>,
    pub start_line: Option<usize>,
    pub end_line: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileReadResult {
    pub files: Vec<ReadFile>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadFile {
    pub path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub total_lines: usize,
    pub truncated: bool,
    pub lines: Vec<NumberedLine>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NumberedLine {
    pub number: usize,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSearchRequest {
    pub query: String,
    pub path: Option<PathBuf>,
    pub depth: Option<usize>,
    pub max_results: Option<usize>,
    pub case_sensitive: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSearchResult {
    pub query: String,
    pub matches: Vec<SearchMatch>,
    pub searched_files: usize,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchMatch {
    pub path: String,
    pub line: usize,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsListRequest {
    pub path: Option<PathBuf>,
    pub depth: Option<usize>,
    pub max_entries: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsListResult {
    pub root: String,
    pub entries: Vec<FsEntry>,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsEntry {
    pub path: String,
    pub kind: String,
    pub depth: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExploreBatchRequest {
    pub goal: String,
    pub probes: Vec<ExploreProbe>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExploreProbeKind {
    List,
    Search,
    Read,
    Terminal,
}

impl ExploreProbeKind {
    pub fn from_name(name: &str) -> Option<Self> {
        match name
            .trim()
            .to_ascii_lowercase()
            .replace(['-', '.'], "_")
            .as_str()
        {
            "list" | "fs_list" => Some(Self::List),
            "search" | "file_search" => Some(Self::Search),
            "read" | "file_read" => Some(Self::Read),
            "terminal" | "terminal_exec" | "shell" => Some(Self::Terminal),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::List => "list",
            Self::Search => "search",
            Self::Read => "read",
            Self::Terminal => "terminal",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExploreProbe {
    pub kind: ExploreProbeKind,
    pub query: Option<String>,
    pub path: Option<PathBuf>,
    pub paths: Vec<PathBuf>,
    pub command: Option<String>,
    pub cwd: Option<PathBuf>,
    pub start_line: Option<usize>,
    pub end_line: Option<usize>,
    pub depth: Option<usize>,
    pub max_results: Option<usize>,
    pub max_entries: Option<usize>,
    pub case_sensitive: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExploreBatchResult {
    pub goal: String,
    pub probes: Vec<ExploreProbeResult>,
    pub failed: usize,
    pub elapsed_ms: u128,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExploreProbeResult {
    pub index: usize,
    pub kind: String,
    pub label: String,
    pub failed: bool,
    pub output: String,
    pub elapsed_ms: u128,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePatchRequest {
    pub diff: String,
    pub cwd: Option<PathBuf>,
    pub description: Option<String>,
}

impl FilePatchRequest {
    pub fn new(diff: impl Into<String>) -> Self {
        Self {
            diff: diff.into(),
            cwd: None,
            description: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePatchResult {
    pub changed_files: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEditRequest {
    pub path: PathBuf,
    pub old_string: String,
    pub new_string: String,
    pub replace_all: bool,
}

impl FileEditRequest {
    pub fn new(
        path: impl Into<PathBuf>,
        old_string: impl Into<String>,
        new_string: impl Into<String>,
    ) -> Self {
        Self {
            path: path.into(),
            old_string: old_string.into(),
            new_string: new_string.into(),
            replace_all: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEditResult {
    pub path: String,
    pub replacements: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskUpdateRequest {
    pub status: String,
}

impl TaskUpdateRequest {
    pub fn new(status: impl Into<String>) -> Self {
        Self {
            status: status.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskUpdateResult {
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanUpdateRequest {
    pub summary: Option<String>,
    pub items: Vec<PlanUpdateItem>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanUpdateItem {
    pub text: String,
    pub status: String,
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanUpdateResult {
    pub summary: String,
    pub items: Vec<PlanUpdateItem>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionRequest {
    pub question: String,
}

impl QuestionRequest {
    pub fn new(question: impl Into<String>) -> Self {
        Self {
            question: question.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionResult {
    pub question: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionRequest {
    pub title: Option<String>,
    pub reason: Option<String>,
    pub questions: Vec<DecisionQuestionRequest>,
    pub assumptions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionQuestionRequest {
    pub id: String,
    pub prompt: String,
    pub kind: String,
    pub options: Vec<String>,
    pub recommended: Option<String>,
    pub required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionResult {
    pub title: String,
    pub reason: String,
    pub questions: Vec<DecisionQuestion>,
    pub assumptions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionQuestion {
    pub id: String,
    pub prompt: String,
    pub kind: String,
    pub options: Vec<String>,
    pub recommended: Option<String>,
    pub required: bool,
}

fn normalize_plan_status(status: &str) -> Option<&'static str> {
    match status
        .trim()
        .to_ascii_lowercase()
        .replace(['_', '-'], " ")
        .as_str()
    {
        "pending" | "todo" | "queued" => Some("pending"),
        "active" | "in progress" | "current" | "doing" => Some("active"),
        "done" | "complete" | "completed" | "succeeded" => Some("done"),
        "blocked" | "failed" | "stuck" => Some("blocked"),
        _ => None,
    }
}

fn normalize_decision_kind(kind: &str) -> &'static str {
    match kind
        .trim()
        .to_ascii_lowercase()
        .replace(['_', '-'], " ")
        .as_str()
    {
        "text" | "free text" | "freeform" | "free form" => "text",
        "choice" | "single choice" | "single" => "choice",
        _ => "choice",
    }
}

fn sanitize_decision_id(id: &str) -> Option<String> {
    let id = id
        .trim()
        .chars()
        .filter_map(|ch| {
            if ch.is_ascii_alphanumeric() {
                Some(ch.to_ascii_lowercase())
            } else if matches!(ch, '-' | '_' | ' ') {
                Some('_')
            } else {
                None
            }
        })
        .collect::<String>()
        .trim_matches('_')
        .chars()
        .take(48)
        .collect::<String>();
    (!id.is_empty()).then_some(id)
}

fn run_explore_probe(tools: &ToolRuntime, index: usize, probe: ExploreProbe) -> ExploreProbeResult {
    let started = Instant::now();
    let kind = probe.kind.as_str().to_string();
    let label = explore_probe_label(&probe);
    let result = match probe.kind {
        ExploreProbeKind::List => {
            let request = FsListRequest {
                path: probe.path,
                depth: probe.depth,
                max_entries: probe.max_entries.or(probe.max_results),
            };
            tools
                .fs_list(request)
                .map(|result| summarize_list_evidence(&result))
        }
        ExploreProbeKind::Search => {
            let query = probe.query.unwrap_or_default();
            let request = FileSearchRequest {
                query,
                path: probe.path,
                depth: probe.depth,
                max_results: probe.max_results,
                case_sensitive: probe.case_sensitive,
            };
            tools
                .file_search(request)
                .map(|result| summarize_search_evidence(&result))
        }
        ExploreProbeKind::Read => {
            let request = FileReadRequest {
                paths: probe.paths,
                start_line: probe.start_line,
                end_line: probe.end_line,
            };
            tools
                .file_read(request)
                .map(|result| summarize_read_evidence(&result))
        }
        ExploreProbeKind::Terminal => {
            let command = probe.command.unwrap_or_default();
            validate_explore_terminal_command(&command)
                .and_then(|_| {
                    tools.terminal_exec(TerminalExecRequest {
                        command,
                        cwd: probe.cwd,
                        background: false,
                    })
                })
                .map(|result| summarize_terminal_evidence(&result))
        }
    };

    match result {
        Ok(output) => ExploreProbeResult {
            index,
            kind,
            label,
            failed: false,
            output,
            elapsed_ms: started.elapsed().as_millis(),
        },
        Err(error) => ExploreProbeResult {
            index,
            kind,
            label,
            failed: true,
            output: format!("error: {error}"),
            elapsed_ms: started.elapsed().as_millis(),
        },
    }
}

fn explore_probe_label(probe: &ExploreProbe) -> String {
    match probe.kind {
        ExploreProbeKind::List => probe
            .path
            .as_ref()
            .map(|path| format!("list {}", path.display()))
            .unwrap_or_else(|| "list workspace".to_string()),
        ExploreProbeKind::Search => probe
            .query
            .as_deref()
            .map(|query| format!("search {query:?}"))
            .unwrap_or_else(|| "search files".to_string()),
        ExploreProbeKind::Read => {
            if probe.paths.len() == 1 {
                format!("read {}", probe.paths[0].display())
            } else {
                format!("read {} files", probe.paths.len())
            }
        }
        ExploreProbeKind::Terminal => probe
            .command
            .as_deref()
            .map(|command| format!("$ {command}"))
            .unwrap_or_else(|| "run read-only command".to_string()),
    }
}

fn summarize_list_evidence(result: &FsListResult) -> String {
    let mut output = format!(
        "root: {}{}\nentries: {}\n",
        result.root,
        if result.truncated { " (truncated)" } else { "" },
        result.entries.len()
    );
    for entry in result.entries.iter().take(40) {
        output.push_str(&format!(
            "{}{} {}\n",
            "  ".repeat(entry.depth),
            entry.kind,
            entry.path
        ));
    }
    if result.entries.len() > 40 {
        output.push_str(&format!("... {} more entries\n", result.entries.len() - 40));
    }
    compact_text(&output, 6000)
}

fn summarize_search_evidence(result: &FileSearchResult) -> String {
    let mut output = format!(
        "query: {}\nsearched files: {}\nmatches: {}{}\n",
        result.query,
        result.searched_files,
        result.matches.len(),
        if result.truncated { " (truncated)" } else { "" }
    );
    for hit in result.matches.iter().take(40) {
        output.push_str(&format!("{}:{}: {}\n", hit.path, hit.line, hit.text));
    }
    if result.matches.len() > 40 {
        output.push_str(&format!("... {} more matches\n", result.matches.len() - 40));
    }
    compact_text(&output, 7000)
}

fn summarize_read_evidence(result: &FileReadResult) -> String {
    let mut output = format!("read files: {}\n", result.files.len());
    for file in &result.files {
        output.push_str(&format!(
            "{}:{}-{} / {} lines{}\n",
            file.path,
            file.start_line,
            file.end_line,
            file.total_lines,
            if file.truncated { " (truncated)" } else { "" }
        ));
        for line in file.lines.iter().take(120) {
            output.push_str(&format!("{:>5} | {}\n", line.number, line.text));
        }
        if file.lines.len() > 120 {
            output.push_str(&format!("... {} more lines\n", file.lines.len() - 120));
        }
    }
    compact_text(&output, 10_000)
}

fn summarize_terminal_evidence(result: &TerminalExecResult) -> String {
    let mut output = format!("exit: {}\n", result.code.unwrap_or(-1));
    if !result.stdout.trim().is_empty() {
        output.push_str("stdout:\n");
        output.push_str(&result.stdout);
        if !result.stdout.ends_with('\n') {
            output.push('\n');
        }
    }
    if !result.stderr.trim().is_empty() {
        output.push_str("stderr:\n");
        output.push_str(&result.stderr);
        if !result.stderr.ends_with('\n') {
            output.push('\n');
        }
    }
    if result.stdout.trim().is_empty() && result.stderr.trim().is_empty() {
        output.push_str("output: <empty>\n");
    }
    compact_text(&output, 6000)
}

fn validate_explore_terminal_command(command: &str) -> Result<()> {
    let command = command.trim();
    if command.is_empty() {
        bail!("explore terminal probe requires command");
    }

    let forbidden_fragments = [
        "\n",
        ";",
        ">",
        "<",
        "`",
        "$(",
        " rm ",
        " rm -",
        "mv ",
        "cp ",
        "touch ",
        "mkdir ",
        "rmdir ",
        "chmod ",
        "chown ",
        "sed -i",
        "perl -pi",
        "tee ",
        "npm install",
        "pnpm install",
        "yarn add",
        "cargo add",
        "git reset",
        "git checkout",
        "git clean",
        "git apply",
        "git commit",
        "git push",
    ];
    let padded = format!(" {command} ");
    for fragment in forbidden_fragments {
        if command.contains(fragment) || padded.contains(fragment) {
            bail!(
                "terminal probe is not read-only enough for explore_batch: contains `{fragment}`"
            );
        }
    }

    if command.starts_with("find ") && command.contains(" -delete") {
        bail!("terminal probe is not read-only enough for explore_batch: find -delete");
    }
    if command.starts_with("cargo fmt") && !command.starts_with("cargo fmt --check") {
        bail!(
            "terminal probe is not read-only enough for explore_batch: cargo fmt must use --check"
        );
    }

    let allowed_prefixes = [
        "pwd",
        "ls",
        "cat ",
        "sed -n ",
        "rg ",
        "grep ",
        "find ",
        "git status",
        "git diff",
        "git log",
        "cargo check",
        "cargo test",
        "cargo clippy",
        "cargo fmt --check",
        "npm test",
        "npm run test",
        "pnpm test",
        "pnpm run test",
        "yarn test",
    ];
    if !allowed_prefixes
        .iter()
        .any(|prefix| command == *prefix || command.starts_with(prefix))
    {
        bail!("terminal probe is not in the explore_batch read-only allowlist");
    }

    Ok(())
}

fn compact_text(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let compacted = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{compacted}...")
    } else {
        compacted
    }
}

fn run_git_apply(cwd: &Path, diff: &str, check: bool, recount: bool) -> Result<()> {
    let mut command = Command::new("git");
    command.arg("apply");
    if check {
        command.arg("--check");
    }
    if recount {
        command.arg("--recount");
    }

    let mut child = command
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .wrap_err("failed to start git apply")?;

    child
        .stdin
        .as_mut()
        .expect("stdin was configured")
        .write_all(diff.as_bytes())
        .wrap_err("failed to send patch to git apply")?;

    let output = child.wait_with_output().wrap_err("git apply failed")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git apply rejected patch: {}", stderr.trim());
    }

    Ok(())
}

fn normalize_patch(diff: &str) -> String {
    let mut diff = diff.trim().to_string();

    if diff.starts_with("```") {
        let mut lines = diff.lines().collect::<Vec<_>>();
        if lines
            .first()
            .is_some_and(|line| line.trim_start().starts_with("```"))
        {
            lines.remove(0);
        }
        if lines
            .last()
            .is_some_and(|line| line.trim_start().starts_with("```"))
        {
            lines.pop();
        }
        diff = lines.join("\n");
    }

    if let Some(start) = diff.find("diff --git ") {
        diff = diff[start..].to_string();
    }

    if !diff.ends_with('\n') {
        diff.push('\n');
    }

    diff
}

fn is_codex_patch(diff: &str) -> bool {
    diff.trim_start().starts_with("*** Begin Patch")
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PatchOp {
    Add {
        path: String,
        content: String,
    },
    Delete {
        path: String,
    },
    Update {
        path: String,
        move_to: Option<String>,
        hunks: Vec<PatchHunk>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PatchHunk {
    old: String,
    new: String,
}

fn apply_codex_patch(workspace: &Path, diff: &str) -> Result<Vec<String>> {
    let ops = parse_codex_patch(diff)?;
    let mut changed = BTreeSet::new();

    for op in ops {
        match op {
            PatchOp::Add { path, content } => {
                validate_relative_path(&path)?;
                let resolved = workspace.join(&path);
                if resolved.exists() {
                    bail!("Add File target already exists: {path}");
                }
                if let Some(parent) = resolved.parent() {
                    fs::create_dir_all(parent)
                        .wrap_err_with(|| format!("failed to create {}", parent.display()))?;
                }
                fs::write(&resolved, content)
                    .wrap_err_with(|| format!("failed to write {}", resolved.display()))?;
                changed.insert(path);
            }
            PatchOp::Delete { path } => {
                validate_relative_path(&path)?;
                let resolved = workspace.join(&path);
                if !resolved.is_file() {
                    bail!("Delete File target is not a file: {path}");
                }
                fs::remove_file(&resolved)
                    .wrap_err_with(|| format!("failed to delete {}", resolved.display()))?;
                changed.insert(path);
            }
            PatchOp::Update {
                path,
                move_to,
                hunks,
            } => {
                validate_relative_path(&path)?;
                if let Some(ref target) = move_to {
                    validate_relative_path(target)?;
                }
                let resolved = workspace.join(&path);
                if !resolved.is_file() {
                    bail!("Update File target is not a file: {path}");
                }
                let mut content = fs::read_to_string(&resolved)
                    .wrap_err_with(|| format!("failed to read {}", resolved.display()))?;
                for hunk in hunks {
                    if hunk.old.is_empty() {
                        bail!("Update File hunk for {path} has no removable/context lines");
                    }
                    let Some(index) = content.find(&hunk.old) else {
                        bail!(
                            "Update File hunk did not match current content in {path}; re-read the file and retry with exact context"
                        );
                    };
                    content.replace_range(index..index + hunk.old.len(), &hunk.new);
                }

                let final_path = move_to.unwrap_or_else(|| path.clone());
                let final_resolved = workspace.join(&final_path);
                if final_path != path {
                    if let Some(parent) = final_resolved.parent() {
                        fs::create_dir_all(parent)
                            .wrap_err_with(|| format!("failed to create {}", parent.display()))?;
                    }
                    fs::remove_file(&resolved).wrap_err_with(|| {
                        format!("failed to remove moved source {}", resolved.display())
                    })?;
                }
                fs::write(&final_resolved, content)
                    .wrap_err_with(|| format!("failed to write {}", final_resolved.display()))?;
                changed.insert(path);
                changed.insert(final_path);
            }
        }
    }

    Ok(changed.into_iter().collect())
}

fn parse_codex_patch(diff: &str) -> Result<Vec<PatchOp>> {
    let lines = diff.lines().collect::<Vec<_>>();
    if lines.first().map(|line| line.trim()) != Some("*** Begin Patch") {
        bail!("Codex patch must start with *** Begin Patch");
    }
    let mut index = 1;
    let mut ops = Vec::new();
    while index < lines.len() {
        let line = lines[index].trim_end();
        if line == "*** End Patch" {
            return Ok(ops);
        }
        if line == "*** End of File" {
            index += 1;
            continue;
        }
        if let Some(path) = line.strip_prefix("*** Add File: ") {
            index += 1;
            let mut content = String::new();
            while index < lines.len() && !lines[index].starts_with("*** ") {
                let line = lines[index];
                content.push_str(line.strip_prefix('+').unwrap_or(line));
                content.push('\n');
                index += 1;
            }
            ops.push(PatchOp::Add {
                path: path.trim().to_string(),
                content,
            });
            continue;
        }
        if let Some(path) = line.strip_prefix("*** Delete File: ") {
            ops.push(PatchOp::Delete {
                path: path.trim().to_string(),
            });
            index += 1;
            continue;
        }
        if let Some(path) = line.strip_prefix("*** Update File: ") {
            index += 1;
            let mut move_to = None;
            let mut hunks = Vec::new();
            if index < lines.len()
                && let Some(target) = lines[index].trim_end().strip_prefix("*** Move to: ")
            {
                move_to = Some(target.trim().to_string());
                index += 1;
            }
            while index < lines.len() && !lines[index].starts_with("*** ") {
                if lines[index].starts_with("@@") {
                    index += 1;
                }
                let mut old = String::new();
                let mut new = String::new();
                while index < lines.len()
                    && !lines[index].starts_with("@@")
                    && !lines[index].starts_with("*** ")
                {
                    let line = lines[index];
                    if let Some(rest) = line.strip_prefix('-') {
                        old.push_str(rest);
                        old.push('\n');
                    } else if let Some(rest) = line.strip_prefix('+') {
                        new.push_str(rest);
                        new.push('\n');
                    } else {
                        let rest = line.strip_prefix(' ').unwrap_or(line);
                        old.push_str(rest);
                        old.push('\n');
                        new.push_str(rest);
                        new.push('\n');
                    }
                    index += 1;
                }
                if !old.is_empty() || !new.is_empty() {
                    hunks.push(PatchHunk { old, new });
                }
            }
            if hunks.is_empty() && move_to.is_none() {
                bail!("Update File requires at least one hunk or Move to: {path}");
            }
            ops.push(PatchOp::Update {
                path: path.trim().to_string(),
                move_to,
                hunks,
            });
            continue;
        }
        bail!("unrecognized Codex patch line: {line}");
    }
    bail!("Codex patch missing *** End Patch")
}

fn extract_patch_paths(diff: &str) -> Result<Vec<String>> {
    let mut paths = BTreeSet::new();

    if is_codex_patch(diff) {
        for op in parse_codex_patch(diff)? {
            match op {
                PatchOp::Add { path, .. } | PatchOp::Delete { path } => {
                    paths.insert(path);
                }
                PatchOp::Update { path, move_to, .. } => {
                    paths.insert(path);
                    if let Some(move_to) = move_to {
                        paths.insert(move_to);
                    }
                }
            }
        }
        return Ok(paths.into_iter().collect());
    }

    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            let mut parts = rest.split_whitespace();
            let _old = parts.next();
            if let Some(new) = parts.next().and_then(strip_git_prefix) {
                paths.insert(new.to_string());
            }
            continue;
        }

        if let Some(path) = line
            .strip_prefix("+++ ")
            .or_else(|| line.strip_prefix("--- "))
            .and_then(strip_git_prefix)
            .filter(|path| *path != "/dev/null")
        {
            paths.insert(path.to_string());
        }
    }

    Ok(paths.into_iter().collect())
}

fn strip_git_prefix(path: &str) -> Option<&str> {
    path.strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .or(Some(path))
}

fn validate_relative_path(path: &str) -> Result<()> {
    let path = Path::new(path);

    if path.is_absolute() {
        bail!("patch path must be relative: {}", path.display());
    }

    for component in path.components() {
        match component {
            Component::Normal(_) => {}
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!("patch path escapes workspace: {}", path.display());
            }
        }
    }

    Ok(())
}

fn normalize_workspace_relative_path(path: &Path) -> Result<String> {
    let mut normalized = PathBuf::new();

    for component in path.components() {
        match component {
            Component::Normal(part) => normalized.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!("patch path escapes workspace: {}", path.display());
            }
        }
    }

    Ok(normalized
        .to_string_lossy()
        .trim_start_matches('/')
        .to_string()
        .if_empty("."))
}

fn sorted_read_dir(path: &Path) -> Result<Vec<fs::DirEntry>> {
    let mut entries = fs::read_dir(path)
        .wrap_err_with(|| format!("failed to list {}", path.display()))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .wrap_err_with(|| format!("failed to read directory entry in {}", path.display()))?;
    entries.sort_by_key(|entry| entry.path());
    Ok(entries)
}

fn should_skip_dir(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(OsStr::to_str),
        Some(
            ".git"
                | ".hg"
                | ".svn"
                | ".medusa"
                | ".next"
                | ".turbo"
                | ".venv"
                | "__pycache__"
                | "build"
                | "coverage"
                | "dist"
                | "node_modules"
                | "target"
        )
    )
}

trait IfEmpty {
    fn if_empty(self, fallback: &str) -> String;
}

impl IfEmpty for String {
    fn if_empty(self, fallback: &str) -> String {
        if self.is_empty() {
            fallback.to_string()
        } else {
            self
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn terminal_exec_runs_command() {
        let runtime = ToolRuntime::new(std::env::current_dir().unwrap()).unwrap();

        let result = runtime
            .terminal_exec(TerminalExecRequest::new("printf medusa"))
            .unwrap();

        assert_eq!(result.code, Some(0));
        assert_eq!(result.stdout, "medusa");
    }

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
            })
            .unwrap();

        assert_eq!(result.matches.len(), 2);
        assert!(result.matches.iter().any(|hit| hit.path == "README.md"));
        assert!(result.matches.iter().any(|hit| hit.path == "src/main.rs"));
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

    #[test]
    fn task_update_trims_status() {
        let runtime = ToolRuntime::new(std::env::current_dir().unwrap()).unwrap();

        let result = runtime
            .task_update(TaskUpdateRequest::new("  running tests  "))
            .unwrap();

        assert_eq!(result.status, "running tests");
    }

    #[test]
    fn plan_update_normalizes_status_and_rejects_multiple_active_steps() {
        let runtime = ToolRuntime::new(std::env::current_dir().unwrap()).unwrap();

        let result = runtime
            .plan_update(PlanUpdateRequest {
                summary: Some("  Ship plan view  ".to_string()),
                items: vec![
                    PlanUpdateItem {
                        text: "  inspect current TUI  ".to_string(),
                        status: "completed".to_string(),
                        evidence: vec![" main.rs ".to_string()],
                    },
                    PlanUpdateItem {
                        text: "render plan modal".to_string(),
                        status: "in-progress".to_string(),
                        evidence: Vec::new(),
                    },
                ],
            })
            .unwrap();

        assert_eq!(result.summary, "Ship plan view");
        assert_eq!(result.items[0].status, "done");
        assert_eq!(result.items[1].status, "active");
        assert_eq!(result.items[0].evidence, vec!["main.rs"]);

        let error = runtime
            .plan_update(PlanUpdateRequest {
                summary: None,
                items: vec![
                    PlanUpdateItem {
                        text: "one".to_string(),
                        status: "active".to_string(),
                        evidence: Vec::new(),
                    },
                    PlanUpdateItem {
                        text: "two".to_string(),
                        status: "doing".to_string(),
                        evidence: Vec::new(),
                    },
                ],
            })
            .unwrap_err();

        assert!(error.to_string().contains("at most one"));
    }

    #[test]
    fn question_trims_and_limits_text() {
        let runtime = ToolRuntime::new(std::env::current_dir().unwrap()).unwrap();

        let result = runtime
            .question(QuestionRequest::new("  Which branch should I keep?  "))
            .unwrap();

        assert_eq!(result.question, "Which branch should I keep?");
    }

    #[test]
    fn decision_request_normalizes_questions_and_requires_choice_options() {
        let runtime = ToolRuntime::new(std::env::current_dir().unwrap()).unwrap();

        let result = runtime
            .decision_request(DecisionRequest {
                title: Some("  Choose storage  ".to_string()),
                reason: Some("  Plan changes persistence.  ".to_string()),
                questions: vec![
                    DecisionQuestionRequest {
                        id: "Storage Model".to_string(),
                        prompt: "Where should plans live?".to_string(),
                        kind: "single_choice".to_string(),
                        options: vec![" transcript ".to_string(), " plan file ".to_string()],
                        recommended: Some(" transcript ".to_string()),
                        required: true,
                    },
                    DecisionQuestionRequest {
                        id: "Storage Model".to_string(),
                        prompt: "Any naming note?".to_string(),
                        kind: "free text".to_string(),
                        options: Vec::new(),
                        recommended: None,
                        required: false,
                    },
                ],
                assumptions: vec!["  Default to transcript.  ".to_string()],
            })
            .unwrap();

        assert_eq!(result.title, "Choose storage");
        assert_eq!(result.reason, "Plan changes persistence.");
        assert_eq!(result.questions[0].id, "storage_model");
        assert_eq!(result.questions[1].id, "storage_model_2");
        assert_eq!(result.questions[0].kind, "choice");
        assert_eq!(result.questions[1].kind, "text");
        assert_eq!(result.questions[0].options, vec!["transcript", "plan file"]);
        assert_eq!(
            result.questions[0].recommended.as_deref(),
            Some("transcript")
        );
        assert_eq!(result.assumptions, vec!["Default to transcript."]);

        let error = runtime
            .decision_request(DecisionRequest {
                title: None,
                reason: None,
                questions: vec![DecisionQuestionRequest {
                    id: "missing".to_string(),
                    prompt: "Choose?".to_string(),
                    kind: "choice".to_string(),
                    options: Vec::new(),
                    recommended: None,
                    required: true,
                }],
                assumptions: Vec::new(),
            })
            .unwrap_err();

        assert!(error.to_string().contains("options is required"));
    }

    fn write_permissions(workspace: &Path, json: &str) {
        fs::create_dir_all(workspace.join(".medusa")).unwrap();
        fs::write(workspace.join(".medusa/permissions.json"), json).unwrap();
    }

    fn temp_workspace() -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("medusa-tools-test-{suffix}"));
        fs::create_dir_all(&path).unwrap();
        path
    }
}
