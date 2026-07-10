//! Per-turn file checkpoints: copy-on-write pre-images captured before the
//! first mutation of each file in a turn, stored under
//! `.medusa/checkpoints/<id>/`, plus a store that lists, restores, and prunes
//! them. Only files changed via the edit/patch tools are captured;
//! shell-command side effects are explicitly out of scope.

use std::{
    collections::BTreeSet,
    fs,
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use color_eyre::eyre::{Result, WrapErr, bail};
use serde::{Deserialize, Serialize};

/// Files larger than this are recorded as `skipped_too_large` instead of
/// copied, so `.medusa/checkpoints` stays bounded.
const MAX_CAPTURED_FILE_BYTES: u64 = 50 * 1024 * 1024;

const DEFAULT_MAX_CHECKPOINTS: usize = 50;
const DEFAULT_MAX_TOTAL_MB: u64 = 200;

/// Caveat stamped into every manifest so on-disk data is self-describing.
const MANIFEST_SCOPE_NOTE: &str =
    "files changed via edit/patch tools only; shell-command changes are not captured";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreImageKind {
    /// The pre-mutation content is stored under `files/<path>`.
    Stored,
    /// The file did not exist before the turn; restore deletes it.
    Absent,
    /// The file exceeded the size cap; restore cannot rewind it.
    SkippedTooLarge,
    /// The path's final component was a symlink; capturing would dereference
    /// it (copying an arbitrarily large, possibly out-of-workspace target into
    /// `.medusa`). It is recorded but never rewound.
    SkippedSymlink,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilePreImage {
    pub path: String,
    pub pre: PreImageKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointMeta {
    pub session_id: String,
    pub prompt_excerpt: String,
    pub transcript_user_index: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointSummary {
    pub id: String,
    pub file_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointEntry {
    pub id: String,
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub created_at_ms: u64,
    #[serde(default)]
    pub prompt_excerpt: String,
    #[serde(default)]
    pub transcript_user_index: usize,
    /// Id of the newest checkpoint that existed when this one was created;
    /// restore uses it to detect manually broken chains.
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub files: Vec<FilePreImage>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RestoreReport {
    pub restored: Vec<String>,
    pub deleted: Vec<String>,
    /// Files recorded as too large / captured through a symlink; NOT rewound.
    pub skipped: Vec<String>,
    /// Manifest entries refused because they would escape the workspace (an
    /// absolute/`..` path, or one reached through an out-of-workspace
    /// symlink). NEVER written to or deleted — surfaced so the user knows the
    /// entry was ignored rather than silently applied against a host file.
    pub refused: Vec<String>,
    /// Id of the pre-rewind safety checkpoint, when one was created.
    pub safety_checkpoint: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PruneReport {
    pub removed: Vec<String>,
    pub kept: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionLimits {
    pub max_checkpoints: usize,
    pub max_total_bytes: u64,
}

impl Default for RetentionLimits {
    fn default() -> Self {
        Self {
            max_checkpoints: DEFAULT_MAX_CHECKPOINTS,
            max_total_bytes: DEFAULT_MAX_TOTAL_MB * 1024 * 1024,
        }
    }
}

impl RetentionLimits {
    /// Defaults of newest 50 checkpoints / 200 MB total, overridable via
    /// `MEDUSA_CHECKPOINT_MAX` and `MEDUSA_CHECKPOINT_MAX_MB`.
    pub fn from_env() -> Self {
        let mut limits = Self::default();
        if let Ok(value) = std::env::var("MEDUSA_CHECKPOINT_MAX")
            && let Ok(count) = value.trim().parse::<usize>()
            && count > 0
        {
            limits.max_checkpoints = count;
        }
        if let Ok(value) = std::env::var("MEDUSA_CHECKPOINT_MAX_MB")
            && let Ok(megabytes) = value.trim().parse::<u64>()
            && megabytes > 0
        {
            limits.max_total_bytes = megabytes * 1024 * 1024;
        }
        limits
    }
}

fn checkpoints_dir(workspace: &Path) -> PathBuf {
    workspace.join(".medusa").join("checkpoints")
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Sortable checkpoint id: zero-padded millis keep lexicographic order equal
/// to chronological order; pid + atomic counter disambiguate same-millisecond
/// checkpoints across processes (never a bare timestamp — that raced before).
fn checkpoint_id(created_at_ms: u64) -> String {
    static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let counter = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("cp-{created_at_ms:013}-{}-{counter:04}", std::process::id())
}

/// Normalize a workspace-relative path for capture: reject absolute or
/// escaping paths and drop `.` components so `./src/a.rs` and `src/a.rs`
/// dedupe to one pre-image.
fn normalize_capture_path(path: &str) -> Result<String> {
    let raw = Path::new(path);
    if raw.is_absolute() {
        bail!("checkpoint path must be workspace-relative: {path}");
    }
    let mut normalized = PathBuf::new();
    for component in raw.components() {
        match component {
            Component::Normal(part) => normalized.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!("checkpoint path escapes workspace: {path}");
            }
        }
    }
    let normalized = normalized.to_string_lossy().to_string();
    if normalized.is_empty() {
        bail!("checkpoint path is empty: {path}");
    }
    Ok(normalized)
}

fn is_medusa_internal(path: &str) -> bool {
    // Check the first REAL path component, not the raw string: `./.medusa/x`
    // and `.//.medusa` must be caught too (a string prefix check on the raw
    // path misses the `./` form and would let a crafted manifest overwrite
    // another checkpoint's data).
    Path::new(path)
        .components()
        .find_map(|component| match component {
            Component::CurDir => None,
            Component::Normal(name) => Some(name == std::ffi::OsStr::new(".medusa")),
            _ => Some(false),
        })
        == Some(true)
}

/// Validate that a manifest path is safe to write/delete during a restore and
/// return its absolute target. A checkpoint manifest is untrusted on-disk JSON,
/// so restore must NEVER trust its paths: an entry may be absolute, contain
/// `..`, name `.medusa` internals, or route through a symlink whose real
/// location is outside the workspace (deleting/overwriting host files). Any of
/// those is refused. `workspace_canonical` must be the canonicalized root.
fn resolve_restore_target(
    workspace: &Path,
    workspace_canonical: &Path,
    path: &str,
) -> std::result::Result<PathBuf, String> {
    // 1. Lexical: workspace-relative, no `..`/root/prefix components.
    let raw = Path::new(path);
    if raw.is_absolute() {
        return Err("absolute path".to_string());
    }
    for component in raw.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err("contains '..' or a root/prefix component".to_string());
            }
        }
    }
    // 2. Never touch `.medusa` internals — one checkpoint must not corrupt
    //    another's data (capture already skips these; restore must too).
    if is_medusa_internal(path) {
        return Err("path is inside .medusa".to_string());
    }

    let target = workspace.join(path);
    // 3. The parent's REAL location must stay inside the workspace. Canonicalize
    //    the deepest existing ancestor (this resolves every intermediate
    //    symlink) and require it under the canonical root, so a component like
    //    `home -> ~` is caught before any fs operation follows it.
    let mut probe = target.parent().unwrap_or(workspace).to_path_buf();
    let real_parent = loop {
        match probe.canonicalize() {
            Ok(canonical) => break canonical,
            Err(_) => match probe.parent() {
                Some(up) if up != probe => probe = up.to_path_buf(),
                _ => return Err("parent directory cannot be resolved".to_string()),
            },
        }
    };
    if !real_parent.starts_with(workspace_canonical) {
        return Err("resolves outside the workspace (symlink escape?)".to_string());
    }

    // 4. Never follow a leaf symlink: `fs::copy` writes THROUGH it (an
    //    overwrite could land outside the workspace) and it must not be
    //    dereferenced. If the leaf itself is a symlink, refuse the entry.
    if let Ok(meta) = fs::symlink_metadata(&target)
        && meta.file_type().is_symlink()
    {
        return Err("target is a symlink".to_string());
    }

    Ok(target)
}

#[derive(Debug)]
struct RecorderState {
    workspace: PathBuf,
    id: String,
    created_at_ms: u64,
    meta: CheckpointMeta,
    files: Vec<FilePreImage>,
    captured: BTreeSet<String>,
    dir_created: bool,
}

/// Records first-write pre-images for one turn. Clones share state, so the
/// per-turn `ToolRuntime` clone and any workflow subagent clones all land in
/// the same checkpoint.
#[derive(Debug, Clone)]
pub struct CheckpointRecorder {
    inner: Arc<Mutex<RecorderState>>,
}

impl CheckpointRecorder {
    pub fn new(workspace: &Path, meta: CheckpointMeta) -> Self {
        let created_at_ms = now_ms();
        Self {
            inner: Arc::new(Mutex::new(RecorderState {
                workspace: workspace.to_path_buf(),
                id: checkpoint_id(created_at_ms),
                created_at_ms,
                meta,
                files: Vec::new(),
                captured: BTreeSet::new(),
                dir_created: false,
            })),
        }
    }

    pub fn id(&self) -> String {
        self.inner.lock().expect("recorder lock").id.clone()
    }

    /// Capture pre-images for `rel_paths` (workspace-relative). First write
    /// wins per path; paths under `.medusa/` are never captured; the
    /// checkpoint directory is only created once something is captured.
    /// Errors must fail the calling mutation (fail-closed).
    pub fn capture(&self, rel_paths: &[String]) -> Result<()> {
        let mut state = self.inner.lock().expect("recorder lock");
        let mut manifest_dirty = false;

        for raw_path in rel_paths {
            let path = normalize_capture_path(raw_path)?;
            if is_medusa_internal(&path) || state.captured.contains(&path) {
                continue;
            }

            if !state.dir_created {
                let dir = checkpoints_dir(&state.workspace).join(&state.id);
                fs::create_dir_all(dir.join("files")).wrap_err_with(|| {
                    format!("failed to create checkpoint dir {}", dir.display())
                })?;
                state.dir_created = true;
            }

            let source = state.workspace.join(&path);
            // symlink_metadata does NOT follow the final component, so a
            // dangling symlink still counts as present.
            let pre = match fs::symlink_metadata(&source) {
                Err(_) => PreImageKind::Absent,
                // Never copy through a symlink. `metadata.len()` on a symlink
                // is the link's own byte length, not the target's, so a link
                // to a multi-GB file slips past the size cap and `fs::copy`
                // (which dereferences) would duplicate the whole target — and
                // possibly out-of-workspace content — into `.medusa`. Refuse
                // to capture it; restore leaves the link untouched.
                Ok(metadata) if metadata.file_type().is_symlink() => PreImageKind::SkippedSymlink,
                Ok(metadata) if metadata.len() > MAX_CAPTURED_FILE_BYTES => {
                    PreImageKind::SkippedTooLarge
                }
                Ok(_) => {
                    let destination = checkpoints_dir(&state.workspace)
                        .join(&state.id)
                        .join("files")
                        .join(&path);
                    if let Some(parent) = destination.parent() {
                        fs::create_dir_all(parent).wrap_err_with(|| {
                            format!("failed to create checkpoint dir {}", parent.display())
                        })?;
                    }
                    // The symlink guard above guarantees `source` is a regular
                    // in-workspace file here, so this copies plain content.
                    fs::copy(&source, &destination).wrap_err_with(|| {
                        format!(
                            "failed to snapshot {} into checkpoint {}",
                            source.display(),
                            destination.display()
                        )
                    })?;
                    PreImageKind::Stored
                }
            };

            state.captured.insert(path.clone());
            state.files.push(FilePreImage { path, pre });
            manifest_dirty = true;
        }

        if manifest_dirty {
            write_manifest(&state)?;
        }
        Ok(())
    }

    /// Returns a summary when anything was captured; `None` means the turn
    /// was read-only and no checkpoint directory exists.
    pub fn finish(&self) -> Option<CheckpointSummary> {
        let state = self.inner.lock().expect("recorder lock");
        if state.files.is_empty() {
            return None;
        }
        Some(CheckpointSummary {
            id: state.id.clone(),
            file_count: state.files.len(),
        })
    }
}

/// Atomic manifest rewrite (temp + rename): a crashed turn still leaves a
/// parseable checkpoint on disk.
fn write_manifest(state: &RecorderState) -> Result<()> {
    let parent_id = newest_checkpoint_id_excluding(&state.workspace, &state.id);
    let entry = CheckpointEntry {
        id: state.id.clone(),
        session_id: state.meta.session_id.clone(),
        created_at_ms: state.created_at_ms,
        prompt_excerpt: state.meta.prompt_excerpt.clone(),
        transcript_user_index: state.meta.transcript_user_index,
        parent_id,
        note: MANIFEST_SCOPE_NOTE.to_string(),
        files: state.files.clone(),
    };
    let dir = checkpoints_dir(&state.workspace).join(&state.id);
    let manifest = dir.join("manifest.json");
    let temp = dir.join("manifest.json.tmp");
    let json = serde_json::to_string_pretty(&entry).wrap_err("failed to encode manifest")?;
    fs::write(&temp, json)
        .wrap_err_with(|| format!("failed to write checkpoint manifest {}", temp.display()))?;
    fs::rename(&temp, &manifest).wrap_err_with(|| {
        format!(
            "failed to finalize checkpoint manifest {}",
            manifest.display()
        )
    })
}

fn newest_checkpoint_id_excluding(workspace: &Path, excluded: &str) -> Option<String> {
    let dir = checkpoints_dir(workspace);
    let mut newest: Option<String> = None;
    for entry in fs::read_dir(&dir).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name == excluded || !entry.path().join("manifest.json").is_file() {
            continue;
        }
        if newest
            .as_deref()
            .is_none_or(|current| name.as_str() > current)
        {
            newest = Some(name);
        }
    }
    newest
}

#[derive(Debug, Clone)]
pub struct CheckpointStore {
    workspace: PathBuf,
}

impl CheckpointStore {
    pub fn open(workspace: &Path) -> Result<Self> {
        if !workspace.is_dir() {
            bail!("workspace does not exist: {}", workspace.display());
        }
        Ok(Self {
            workspace: workspace.to_path_buf(),
        })
    }

    /// All valid checkpoints, newest first (ids sort chronologically).
    pub fn list(&self) -> Result<Vec<CheckpointEntry>> {
        let dir = checkpoints_dir(&self.workspace);
        let mut entries = Vec::new();
        let Ok(read_dir) = fs::read_dir(&dir) else {
            return Ok(entries);
        };
        for dir_entry in read_dir.flatten() {
            let manifest = dir_entry.path().join("manifest.json");
            let Ok(text) = fs::read_to_string(&manifest) else {
                continue;
            };
            let Ok(mut entry) = serde_json::from_str::<CheckpointEntry>(&text) else {
                continue;
            };
            // The directory name is authoritative for chain ordering.
            entry.id = dir_entry.file_name().to_string_lossy().to_string();
            entries.push(entry);
        }
        entries.sort_by(|a, b| b.id.cmp(&a.id));
        Ok(entries)
    }

    /// Restore the workspace to its state just before `checkpoint_id` by
    /// composing pre-images of the target and every newer checkpoint (the
    /// checkpoint closest to the target wins per path). Refuses when the
    /// chain from newest to target has a gap. Captures the current state of
    /// every affected file into a fresh pre-rewind safety checkpoint first,
    /// so the rewind is itself undoable.
    pub fn restore(&self, checkpoint_id: &str) -> Result<RestoreReport> {
        let entries = self.list()?;
        let Some(target_position) = entries.iter().position(|entry| entry.id == checkpoint_id)
        else {
            bail!("checkpoint `{checkpoint_id}` not found");
        };

        // entries is newest-first; the chain covers newest ..= target.
        let chain = &entries[..=target_position];
        for pair in chain.windows(2) {
            let (newer, older) = (&pair[0], &pair[1]);
            if newer.parent_id.as_deref() != Some(older.id.as_str()) {
                bail!(
                    "checkpoint chain is broken between `{}` and `{}` (a checkpoint was deleted?); refusing to restore `{checkpoint_id}`",
                    older.id,
                    newer.id
                );
            }
        }

        // Earliest (closest-to-target) pre-image wins per path: walk the
        // chain oldest-first and let the first writer claim each path.
        let mut winners: Vec<(String, PreImageKind, String)> = Vec::new();
        let mut claimed = BTreeSet::new();
        for entry in chain.iter().rev() {
            for file in &entry.files {
                if claimed.insert(file.path.clone()) {
                    winners.push((file.path.clone(), file.pre, entry.id.clone()));
                }
            }
        }

        if winners.is_empty() {
            return Ok(RestoreReport::default());
        }

        // Validate EVERY target against the (untrusted) manifest BEFORE any
        // disk operation — including the pre-rewind safety capture, which would
        // otherwise itself follow a malicious symlink and copy a host file into
        // `.medusa`. Refused entries are surfaced, never written or deleted.
        let workspace_canonical = self.workspace.canonicalize().wrap_err_with(|| {
            format!("failed to resolve workspace {}", self.workspace.display())
        })?;
        let mut safe_winners: Vec<(String, PreImageKind, String, PathBuf)> = Vec::new();
        let mut refused = Vec::new();
        let mut skipped = Vec::new();
        for (path, pre, source_id) in winners {
            // Skipped kinds (too large / captured through a symlink) touch no
            // disk on restore, so they need no path validation — validating
            // would wrongly refuse a legitimately-skipped symlink pre-image.
            if matches!(
                pre,
                PreImageKind::SkippedTooLarge | PreImageKind::SkippedSymlink
            ) {
                skipped.push(path);
                continue;
            }
            match resolve_restore_target(&self.workspace, &workspace_canonical, &path) {
                Ok(target) => safe_winners.push((path, pre, source_id, target)),
                Err(_reason) => refused.push(path),
            }
        }

        // Pre-rewind safety checkpoint of only the paths we will actually
        // touch (validated safe above).
        let target = &entries[target_position];
        let safety = CheckpointRecorder::new(
            &self.workspace,
            CheckpointMeta {
                session_id: String::new(),
                prompt_excerpt: format!(
                    "pre-rewind of {}",
                    truncate_chars(&target.prompt_excerpt, 60)
                ),
                transcript_user_index: 0,
            },
        );
        let affected = safe_winners
            .iter()
            .map(|(path, _, _, _)| path.clone())
            .collect::<Vec<_>>();
        let safety_checkpoint = if affected.is_empty() {
            None
        } else {
            safety
                .capture(&affected)
                .wrap_err("failed to create pre-rewind safety checkpoint")?;
            safety.finish().map(|summary| summary.id)
        };

        let mut report = RestoreReport {
            safety_checkpoint,
            refused,
            skipped,
            ..RestoreReport::default()
        };
        for (path, pre, source_id, workspace_path) in safe_winners {
            match pre {
                PreImageKind::Stored => {
                    let stored = checkpoints_dir(&self.workspace)
                        .join(&source_id)
                        .join("files")
                        .join(&path);
                    if let Some(parent) = workspace_path.parent() {
                        fs::create_dir_all(parent)
                            .wrap_err_with(|| format!("failed to create {}", parent.display()))?;
                    }
                    fs::copy(&stored, &workspace_path).wrap_err_with(|| {
                        format!(
                            "failed to restore {} from checkpoint {source_id}",
                            workspace_path.display()
                        )
                    })?;
                    report.restored.push(path);
                }
                PreImageKind::Absent => {
                    if workspace_path.exists() {
                        fs::remove_file(&workspace_path).wrap_err_with(|| {
                            format!("failed to delete {}", workspace_path.display())
                        })?;
                    }
                    remove_empty_parents(&self.workspace, &workspace_path);
                    report.deleted.push(path);
                }
                PreImageKind::SkippedTooLarge | PreImageKind::SkippedSymlink => {
                    report.skipped.push(path);
                }
            }
        }

        Ok(report)
    }

    /// Keep the newest `max_checkpoints` and stay under `max_total_bytes`,
    /// deleting oldest first — which preserves the newest contiguous chain
    /// that restore composition depends on.
    pub fn prune(&self, limits: RetentionLimits) -> Result<PruneReport> {
        let entries = self.list()?;
        let dir = checkpoints_dir(&self.workspace);
        let mut sized = entries
            .iter()
            .map(|entry| (entry.id.clone(), dir_size_bytes(&dir.join(&entry.id))))
            .collect::<Vec<_>>();
        // list() is newest-first; prune from the back (oldest).
        let mut total_bytes: u64 = sized.iter().map(|(_, size)| size).sum();
        let mut report = PruneReport::default();

        while sized.len() > limits.max_checkpoints
            || (total_bytes > limits.max_total_bytes && sized.len() > 1)
        {
            let Some((id, size)) = sized.pop() else {
                break;
            };
            fs::remove_dir_all(dir.join(&id))
                .wrap_err_with(|| format!("failed to prune checkpoint {id}"))?;
            total_bytes = total_bytes.saturating_sub(size);
            report.removed.push(id);
        }

        report.kept = sized.len();
        Ok(report)
    }
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

fn dir_size_bytes(path: &Path) -> u64 {
    let mut total = 0;
    let Ok(read_dir) = fs::read_dir(path) else {
        return fs::symlink_metadata(path)
            .map(|meta| meta.len())
            .unwrap_or(0);
    };
    for entry in read_dir.flatten() {
        let entry_path = entry.path();
        if entry_path.is_dir() {
            total += dir_size_bytes(&entry_path);
        } else {
            total += entry.metadata().map(|metadata| metadata.len()).unwrap_or(0);
        }
    }
    total
}

/// Best-effort cleanup of directories left empty by a restore deletion; never
/// removes the workspace root itself.
fn remove_empty_parents(workspace: &Path, deleted: &Path) {
    let mut current = deleted.parent();
    while let Some(dir) = current {
        if dir == workspace || !dir.starts_with(workspace) {
            break;
        }
        if fs::remove_dir(dir).is_err() {
            break;
        }
        current = dir.parent();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_workspace() -> PathBuf {
        static TEMP_COUNTER: std::sync::atomic::AtomicUsize =
            std::sync::atomic::AtomicUsize::new(0);
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let index = TEMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let pid = std::process::id();
        let path =
            std::env::temp_dir().join(format!("medusa-checkpoint-test-{pid}-{suffix}-{index}"));
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
}
