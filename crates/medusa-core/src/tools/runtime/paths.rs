use super::*;

impl ToolRuntime {
    pub fn read_patch_file(&self, path: &str) -> Result<String> {
        let path = self.resolve_workspace_path(Some(Path::new(path)))?;
        fs::read_to_string(&path)
            .wrap_err_with(|| format!("failed to read patch file: {}", path.display()))
    }

    pub(super) fn resolve_workspace_path(&self, path: Option<&Path>) -> Result<PathBuf> {
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

    pub(super) fn workspace_relative(&self, path: &Path) -> String {
        path.strip_prefix(&self.workspace)
            .unwrap_or(path)
            .to_string_lossy()
            .trim_start_matches('/')
            .to_string()
            .if_empty(".")
    }

    pub(super) fn workspace_relative_patch_paths(
        &self,
        cwd: &Path,
        paths: &[String],
    ) -> Result<Vec<String>> {
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
            let normalized_path = normalize_workspace_relative_path(&workspace_path)?;
            // Lexical validation alone is not enough: `home/.gitconfig` where
            // `home` is a symlink to `~` has only Normal components yet its
            // real location is outside the workspace. Reject such paths BEFORE
            // capture snapshots (and copies into .medusa) or git apply writes
            // through the link.
            self.ensure_patch_path_within_workspace(&normalized_path)?;
            normalized.insert(normalized_path);
        }

        Ok(normalized.into_iter().collect())
    }

    /// Refuse a workspace-relative patch path whose real filesystem location
    /// escapes the workspace through a symlink component, or whose final
    /// component is itself a symlink (writing/snapshotting would dereference
    /// it outside the workspace). New paths whose parent does not exist yet
    /// resolve their deepest existing ancestor, which for a legitimate patch
    /// is the workspace root.
    pub(super) fn ensure_patch_path_within_workspace(&self, rel_path: &str) -> Result<()> {
        let target = self.workspace.join(rel_path);
        let mut probe = target.parent().unwrap_or(&self.workspace).to_path_buf();
        let real_parent = loop {
            match probe.canonicalize() {
                Ok(canonical) => break canonical,
                Err(_) => match probe.parent() {
                    Some(up) if up != probe => probe = up.to_path_buf(),
                    _ => bail!("cannot resolve parent of patch path: {rel_path}"),
                },
            }
        };
        if !real_parent.starts_with(&self.workspace) {
            bail!("patch path escapes workspace through a symlink: {rel_path}");
        }
        if let Ok(meta) = fs::symlink_metadata(&target)
            && meta.file_type().is_symlink()
        {
            bail!("patch path is a symlink; refusing to follow it: {rel_path}");
        }
        Ok(())
    }

    pub(super) fn walk_files(&self, root: &Path, max_depth: usize) -> Result<Vec<PathBuf>> {
        let mut files = Vec::new();
        Self::collect_files(root, 0, max_depth, &mut files)?;
        files.sort();
        Ok(files)
    }

    fn collect_files(
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
                Self::collect_files(&entry_path, depth + 1, max_depth, files)?;
            } else if entry_path.is_file() {
                files.push(entry_path);
            }
        }

        Ok(())
    }

    pub(super) fn collect_list_entries(
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
