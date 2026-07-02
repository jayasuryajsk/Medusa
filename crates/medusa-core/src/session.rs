use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    marker::PhantomData,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use color_eyre::eyre::{Result, WrapErr, bail};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionOpenMode {
    New,
    ContinueLast,
    ContinueNamed(String),
}

#[derive(Debug, Clone)]
pub struct SessionStore<T> {
    session_path: PathBuf,
    pointer_path: PathBuf,
    workspace: PathBuf,
    parent_id: Option<String>,
    _marker: PhantomData<T>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub name: String,
    pub parent: String,
    pub size: String,
    pub current: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionTreeEntry {
    pub name: String,
    pub parent: String,
    pub depth: usize,
    pub size: String,
    pub current: bool,
}

#[derive(Debug, Serialize)]
struct SessionFile<T> {
    version: u32,
    session_id: Option<String>,
    parent_id: Option<String>,
    workspace: String,
    transcript: Vec<T>,
    messages: Vec<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
#[serde(bound(deserialize = "T: Deserialize<'de>, L: Deserialize<'de>"))]
pub struct LoadedSessionFile<T, L = serde_json::Value> {
    pub version: u32,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub parent_id: Option<String>,
    pub workspace: String,
    #[serde(default)]
    pub transcript: Vec<T>,
    #[serde(default)]
    pub messages: Vec<L>,
}

#[derive(Debug, Clone)]
struct SessionIndexEntry {
    name: String,
    parent_id: Option<String>,
    size_bytes: u64,
}

impl<T> SessionStore<T>
where
    T: Clone + Serialize + DeserializeOwned,
{
    pub fn open(workspace: &Path, mode: SessionOpenMode) -> Result<Self> {
        let sessions_dir = workspace.join(".medusa").join("sessions");
        fs::create_dir_all(&sessions_dir).wrap_err("failed to create .medusa/sessions")?;
        let pointer_path = sessions_dir.join("last");

        let session_path = match mode {
            SessionOpenMode::New => sessions_dir.join(format!("{}.json", session_timestamp())),
            SessionOpenMode::ContinueLast => {
                let session_id = read_last_session_name(&pointer_path)?;
                existing_session_path(&sessions_dir, &session_id)?
            }
            SessionOpenMode::ContinueNamed(session_id) => {
                existing_session_path(&sessions_dir, &session_id)?
            }
        };

        let parent_id = read_session_file::<T, serde_json::Value>(&session_path)
            .ok()
            .and_then(|session| session.parent_id);
        write_last_pointer(&pointer_path, &session_path)?;

        Ok(Self {
            session_path,
            pointer_path,
            workspace: workspace.to_path_buf(),
            parent_id,
            _marker: PhantomData,
        })
    }

    pub fn current_id(&self) -> String {
        self.session_path
            .file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new("session.json"))
            .to_string_lossy()
            .to_string()
    }

    pub fn current_stem(&self) -> String {
        self.session_path
            .file_stem()
            .unwrap_or_else(|| std::ffi::OsStr::new("session"))
            .to_string_lossy()
            .to_string()
    }

    pub fn parent_id(&self) -> Option<&str> {
        self.parent_id.as_deref()
    }

    pub fn fork(&mut self, transcript: &[T]) -> Result<String> {
        let Some(sessions_dir) = self.session_path.parent().map(Path::to_path_buf) else {
            bail!("session path has no parent directory");
        };
        fs::create_dir_all(&sessions_dir).wrap_err("failed to create sessions directory")?;

        let parent_id = self.current_id();
        let old_path = self.session_path.clone();
        let old_parent_id = self.parent_id.clone();
        let new_path = unique_session_path(&sessions_dir, &old_path);

        self.session_path = new_path;
        self.parent_id = Some(parent_id);

        if let Err(error) = self.save_transcript(transcript) {
            self.session_path = old_path;
            self.parent_id = old_parent_id;
            return Err(error);
        }

        Ok(self.current_id())
    }

    pub fn switch_to(&mut self, session_id: &str) -> Result<Vec<T>> {
        let Some(sessions_dir) = self.session_path.parent().map(Path::to_path_buf) else {
            bail!("session path has no parent directory");
        };
        let new_path = existing_session_path(&sessions_dir, session_id)?;
        let session = read_session_file::<T, serde_json::Value>(&new_path)
            .wrap_err_with(|| format!("failed to parse session {}", new_path.display()))?;
        let transcript = session.transcript;

        self.session_path = new_path;
        self.parent_id = session.parent_id;
        write_last_pointer(&self.pointer_path, &self.session_path)?;

        Ok(transcript)
    }

    pub fn attachment_dir(&self) -> PathBuf {
        let stem = self
            .session_path
            .file_stem()
            .unwrap_or_else(|| std::ffi::OsStr::new("session"));
        self.session_path
            .with_file_name(stem)
            .with_extension("attachments")
    }

    pub fn load_transcript(&self) -> Result<Vec<T>> {
        if !self.session_path.exists() {
            return Ok(Vec::new());
        }
        Ok(
            read_session_file::<T, serde_json::Value>(&self.session_path)
                .wrap_err_with(|| {
                    format!("failed to parse session {}", self.session_path.display())
                })?
                .transcript,
        )
    }

    pub fn load_transcript_with_legacy<L, F>(&self, map_legacy: F) -> Result<Vec<T>>
    where
        L: DeserializeOwned,
        F: Fn(L) -> T,
    {
        if !self.session_path.exists() {
            return Ok(Vec::new());
        }

        let session = read_session_file::<T, L>(&self.session_path)
            .wrap_err_with(|| format!("failed to parse session {}", self.session_path.display()))?;
        if !session.transcript.is_empty() {
            return Ok(session.transcript);
        }

        Ok(session.messages.into_iter().map(map_legacy).collect())
    }

    pub fn save_transcript(&self, transcript: &[T]) -> Result<()> {
        let session = SessionFile {
            version: 3,
            session_id: Some(self.current_id()),
            parent_id: self.parent_id.clone(),
            workspace: self.workspace.to_string_lossy().to_string(),
            transcript: transcript.to_vec(),
            messages: Vec::new(),
        };

        if let Some(parent) = self.session_path.parent() {
            fs::create_dir_all(parent).wrap_err_with(|| {
                format!("failed to create session directory {}", parent.display())
            })?;
        }

        let json = serde_json::to_string_pretty(&session).wrap_err("failed to encode session")?;
        fs::write(&self.session_path, json)
            .wrap_err_with(|| format!("failed to write session {}", self.session_path.display()))?;

        write_last_pointer(&self.pointer_path, &self.session_path)?;
        Ok(())
    }

    pub fn list_sessions(&self) -> Vec<SessionInfo> {
        self.index_sessions()
            .into_iter()
            .map(|entry| {
                let parent = entry
                    .parent_id
                    .as_deref()
                    .map(compact_session_id)
                    .unwrap_or_default();
                SessionInfo {
                    current: if entry.name == self.current_id() {
                        "yes".to_string()
                    } else {
                        String::new()
                    },
                    name: entry.name,
                    parent,
                    size: human_bytes(entry.size_bytes),
                }
            })
            .collect()
    }

    pub fn tree_entries(&self) -> Vec<SessionTreeEntry> {
        let entries = self.index_sessions();
        let known = entries
            .iter()
            .map(|entry| entry.name.clone())
            .collect::<BTreeSet<_>>();
        let by_name = entries
            .iter()
            .map(|entry| (entry.name.clone(), entry.clone()))
            .collect::<BTreeMap<_, _>>();
        let mut children = BTreeMap::<String, Vec<String>>::new();
        let mut roots = Vec::new();

        for entry in &entries {
            match entry
                .parent_id
                .as_ref()
                .filter(|parent| known.contains(*parent))
            {
                Some(parent) => children
                    .entry(parent.clone())
                    .or_default()
                    .push(entry.name.clone()),
                None => roots.push(entry.name.clone()),
            }
        }

        roots.sort_by(|a, b| b.cmp(a));
        for child_names in children.values_mut() {
            child_names.sort_by(|a, b| b.cmp(a));
        }

        let mut rows = Vec::new();
        for root in roots {
            append_tree_entry(
                &root,
                0,
                &by_name,
                &children,
                self.current_id().as_str(),
                &mut rows,
            );
        }

        rows
    }

    fn index_sessions(&self) -> Vec<SessionIndexEntry> {
        let Some(dir) = self.session_path.parent() else {
            return Vec::new();
        };
        let mut sessions = fs::read_dir(dir)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let path = entry.path();
                let name = path.file_name()?.to_string_lossy().to_string();
                if name == "last" {
                    return None;
                }
                let metadata = entry.metadata().ok()?;
                if !metadata.is_file() || !name.ends_with(".json") {
                    return None;
                }
                let parent_id = read_session_file::<serde_json::Value, serde_json::Value>(&path)
                    .ok()
                    .and_then(|session| session.parent_id);
                Some(SessionIndexEntry {
                    name,
                    parent_id,
                    size_bytes: metadata.len(),
                })
            })
            .collect::<Vec<_>>();
        sessions.sort_by(|a, b| b.name.cmp(&a.name));
        sessions
    }
}

pub fn read_session_file<T, L>(path: &Path) -> Result<LoadedSessionFile<T, L>>
where
    T: DeserializeOwned,
    L: DeserializeOwned,
{
    let text = fs::read_to_string(path)
        .wrap_err_with(|| format!("failed to read session {}", path.display()))?;
    serde_json::from_str(&text)
        .wrap_err_with(|| format!("failed to parse session {}", path.display()))
}

pub fn normalize_session_name(session_id: &str) -> Result<String> {
    let trimmed = session_id.trim();
    if trimmed.is_empty() {
        bail!("session name is empty");
    }
    if trimmed == "last"
        || trimmed == "."
        || trimmed == ".."
        || trimmed.contains("..")
        || trimmed.contains('/')
        || trimmed.contains('\\')
        || Path::new(trimmed).is_absolute()
    {
        bail!("invalid session name `{trimmed}`");
    }

    if trimmed.ends_with(".json") {
        Ok(trimmed.to_string())
    } else {
        Ok(format!("{trimmed}.json"))
    }
}

pub fn compact_session_id(session_id: &str) -> String {
    session_id
        .strip_suffix(".json")
        .unwrap_or(session_id)
        .strip_prefix("session-")
        .unwrap_or_else(|| session_id.strip_suffix(".json").unwrap_or(session_id))
        .to_string()
}

pub fn human_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else {
        format!("{} KB", bytes / 1024)
    }
}

fn read_last_session_name(pointer_path: &Path) -> Result<String> {
    let session_id = fs::read_to_string(pointer_path)
        .wrap_err("no previous session found for this workspace")?
        .trim()
        .to_string();
    if session_id.is_empty() {
        bail!("last session pointer is empty");
    }
    normalize_session_name(&session_id)
}

fn existing_session_path(sessions_dir: &Path, session_id: &str) -> Result<PathBuf> {
    let session_file = normalize_session_name(session_id)?;
    let path = sessions_dir.join(&session_file);
    if !path.exists() {
        bail!("session `{session_file}` not found");
    }
    Ok(path)
}

fn write_last_pointer(pointer_path: &Path, session_path: &Path) -> Result<()> {
    fs::write(
        pointer_path,
        session_path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .as_bytes(),
    )
    .wrap_err("failed to update last session pointer")
}

fn unique_session_path(sessions_dir: &Path, excluded: &Path) -> PathBuf {
    for attempt in 0.. {
        let stem = session_timestamp();
        let file_name = if attempt == 0 {
            format!("{stem}.json")
        } else {
            format!("{stem}-{attempt}.json")
        };
        let candidate = sessions_dir.join(file_name);
        if candidate != excluded && !candidate.exists() {
            return candidate;
        }
    }

    unreachable!("unbounded session path search should always return")
}

fn append_tree_entry(
    name: &str,
    depth: usize,
    by_name: &BTreeMap<String, SessionIndexEntry>,
    children: &BTreeMap<String, Vec<String>>,
    current_id: &str,
    rows: &mut Vec<SessionTreeEntry>,
) {
    let Some(entry) = by_name.get(name) else {
        return;
    };
    rows.push(SessionTreeEntry {
        name: entry.name.clone(),
        parent: entry
            .parent_id
            .as_deref()
            .map(compact_session_id)
            .unwrap_or_default(),
        depth,
        size: human_bytes(entry.size_bytes),
        current: entry.name == current_id,
    });

    for child in children.get(name).into_iter().flatten() {
        append_tree_entry(child, depth + 1, by_name, children, current_id, rows);
    }
}

fn session_timestamp() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("session-{millis}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    struct TestMessage {
        text: String,
    }

    #[test]
    fn rejects_traversal_session_names() {
        assert!(normalize_session_name("../session-1").is_err());
        assert!(normalize_session_name("nested/session-1").is_err());
        assert!(normalize_session_name("last").is_err());
    }

    #[test]
    fn named_open_loads_requested_session() {
        let workspace = temp_workspace("named-open");
        let mut session =
            SessionStore::<TestMessage>::open(&workspace, SessionOpenMode::New).unwrap();
        let root_id = session.current_id();
        let root = vec![TestMessage {
            text: "root".into(),
        }];
        let child = vec![TestMessage {
            text: "child".into(),
        }];

        session.save_transcript(&root).unwrap();
        let child_id = session.fork(&child).unwrap();
        assert_ne!(root_id, child_id);

        let named = SessionStore::<TestMessage>::open(
            &workspace,
            SessionOpenMode::ContinueNamed(root_id.trim_end_matches(".json").into()),
        )
        .unwrap();

        assert_eq!(named.current_id(), root_id);
        assert_eq!(named.load_transcript().unwrap(), root);
        assert_eq!(
            fs::read_to_string(workspace.join(".medusa/sessions/last")).unwrap(),
            root_id
        );
    }

    #[test]
    fn tree_entries_include_depth_and_current_session() {
        let workspace = temp_workspace("tree");
        let mut session =
            SessionStore::<TestMessage>::open(&workspace, SessionOpenMode::New).unwrap();
        let root_id = session.current_id();

        session.save_transcript(&[]).unwrap();
        let child_id = session.fork(&[]).unwrap();

        let tree = session.tree_entries();
        let root = tree.iter().find(|entry| entry.name == root_id).unwrap();
        let child = tree.iter().find(|entry| entry.name == child_id).unwrap();

        assert_eq!(root.depth, 0);
        assert_eq!(child.depth, 1);
        assert!(child.current);
    }

    fn temp_workspace(label: &str) -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("medusa-session-{label}-{suffix}"));
        fs::create_dir_all(&path).unwrap();
        path
    }
}
