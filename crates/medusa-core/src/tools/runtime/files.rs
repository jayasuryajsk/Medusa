use super::*;

impl ToolRuntime {
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
        let matcher = SearchMatcher::new(query, case_sensitive);
        let include = request
            .include
            .as_deref()
            .map(compile_include_glob)
            .transpose()?;
        let mut matches = Vec::new();
        let mut searched_files = 0usize;

        for file in self.walk_files(&root, request.depth.unwrap_or(8).clamp(0, 16))? {
            if matches.len() >= max_results {
                break;
            }
            if let Some(include) = &include
                && !include.is_match(self.workspace_relative(&file))
            {
                continue;
            }
            if file.metadata().map(|meta| meta.len()).unwrap_or(0) > 2_000_000 {
                continue;
            }
            let Ok(content) = fs::read_to_string(&file) else {
                continue;
            };
            searched_files += 1;
            for (line_index, line) in content.lines().enumerate() {
                if matcher.is_match(line) {
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
            regex: matcher.is_regex(),
            matches,
            searched_files,
            truncated,
        })
    }

    pub fn file_glob(&self, request: FileGlobRequest) -> Result<FileGlobResult> {
        let pattern = request.pattern.trim();
        if pattern.is_empty() {
            bail!("file_glob.pattern cannot be empty");
        }

        let root = self.resolve_workspace_path(request.path.as_deref())?;
        let max_results = request.max_results.unwrap_or(120).clamp(1, 500);
        let glob = compile_include_glob(pattern)?;

        let mut matched = Vec::new();
        for file in self.walk_files(&root, 16)? {
            let relative = self.workspace_relative(&file);
            if glob.is_match(&relative) {
                let modified = file
                    .metadata()
                    .and_then(|meta| meta.modified())
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                matched.push((relative, modified));
            }
        }

        matched.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let truncated = matched.len() > max_results;
        matched.truncate(max_results);

        Ok(FileGlobResult {
            pattern: pattern.to_string(),
            root: self.workspace_relative(&root),
            paths: matched.into_iter().map(|(path, _)| path).collect(),
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
        self.authorize(
            self.permissions
                .evaluate_patch_paths(&workspace_changed_files),
            || ApprovalRequest {
                tool: ApprovalTool::FilePatch,
                command: None,
                paths: workspace_changed_files.clone(),
                background: false,
                sandbox_escalation: false,
            },
        )?;

        // After approval (denied ops never create checkpoints), before either
        // apply path writes anything.
        self.capture_checkpoint(&workspace_changed_files)?;

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
        self.authorize(
            self.permissions
                .evaluate_patch_paths(std::slice::from_ref(&path)),
            || ApprovalRequest {
                tool: ApprovalTool::FileEdit,
                command: None,
                paths: vec![path.clone()],
                background: false,
                sandbox_escalation: false,
            },
        )?;

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
            // Capture ONLY after confirming the resolved parent is inside the
            // workspace — capturing earlier would snapshot (and copy into
            // .medusa) a file reached through an out-of-workspace symlink,
            // poisoning the manifest with a host path. Records `absent`.
            self.capture_checkpoint(std::slice::from_ref(&path))?;
            atomic_write(&candidate, request.new_string)
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

        // resolve_workspace_path canonicalized and confirmed `resolved` is
        // inside the workspace (rejecting out-of-workspace symlink targets), so
        // it is now safe to snapshot the pre-image before the write.
        self.capture_checkpoint(std::slice::from_ref(&path))?;

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
        atomic_write(&resolved, new_content)
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
}
