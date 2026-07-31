use super::*;

pub(crate) fn sorted_read_dir(path: &Path) -> Result<Vec<fs::DirEntry>> {
    let mut entries = fs::read_dir(path)
        .wrap_err_with(|| format!("failed to list {}", path.display()))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .wrap_err_with(|| format!("failed to read directory entry in {}", path.display()))?;
    entries.sort_by_key(|entry| entry.path());
    Ok(entries)
}

pub(crate) fn should_skip_dir(path: &Path) -> bool {
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

pub(crate) trait IfEmpty {
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
