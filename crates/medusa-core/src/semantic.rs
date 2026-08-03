//! Incremental, repository-local semantic code search backed by llama.cpp.

use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    env, fs,
    io::Write,
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, UNIX_EPOCH},
};

use color_eyre::eyre::{Result, WrapErr, bail, eyre};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    cancel::CancelToken,
    tools::{SemanticMatch, SemanticSearchRequest, SemanticSearchResult},
};

const INDEX_VERSION: u32 = 1;
const MODEL_NAME: &str = "nomic-embed-text-v1.5.Q8_0.gguf";
const MODEL_ID: &str = "nomic-ai/nomic-embed-text-v1.5.Q8_0";
const MODEL_URL: &str = "https://huggingface.co/nomic-ai/nomic-embed-text-v1.5-GGUF/resolve/393a6bc2204f0ab8afc53cc877ae52c15a8553f8/nomic-embed-text-v1.5.Q8_0.gguf";
const MAX_FILE_BYTES: u64 = 2_000_000;
const CHUNK_TARGET_CHARS: usize = 3_600;
const CHUNK_MAX_LINES: usize = 120;
const CHUNK_OVERLAP_LINES: usize = 12;

#[derive(Debug)]
pub struct SemanticRuntime {
    workspace: PathBuf,
    state: Mutex<SemanticState>,
    server: Mutex<Option<Arc<EmbeddingServer>>>,
}

#[derive(Debug, Default)]
struct SemanticState {
    index: Option<StoredIndex>,
}

impl SemanticRuntime {
    pub fn new(workspace: PathBuf) -> Self {
        Self {
            workspace,
            state: Mutex::new(SemanticState::default()),
            server: Mutex::new(None),
        }
    }

    pub fn search(
        &self,
        request: SemanticSearchRequest,
        cancel: &CancelToken,
    ) -> Result<SemanticSearchResult> {
        let query = request.query.trim();
        if query.is_empty() {
            bail!("semantic_search.query cannot be empty");
        }

        let mut state = self
            .state
            .lock()
            .map_err(|_| eyre!("semantic index lock was poisoned"))?;
        if state.index.is_none() {
            state.index = Some(load_index(&self.index_path()).unwrap_or_default());
        }

        let server = self.embedding_server(cancel)?;
        let query_embedding = server.embed(&[format!("search_query: {query}")], cancel)?;
        let query_embedding = query_embedding
            .into_iter()
            .next()
            .ok_or_else(|| eyre!("embedding server returned no query vector"))?;
        let query_embedding = normalize(query_embedding);
        if state
            .index
            .as_ref()
            .is_some_and(|index| index.dimensions != 0 && index.dimensions != query_embedding.len())
        {
            state.index = Some(StoredIndex::default());
        }

        let updated_files = refresh_index(
            state.index.as_mut().expect("index initialized"),
            &self.workspace,
            &server,
            cancel,
        )?;
        if updated_files > 0 || !self.index_path().exists() {
            save_index(
                &self.index_path(),
                state.index.as_ref().expect("index initialized"),
            )?;
        }
        cancel.bail_if_cancelled()?;
        let max_results = request.max_results.unwrap_or(8).clamp(1, 24);
        let path_filter = request
            .path
            .as_deref()
            .map(normalize_filter_path)
            .transpose()?;
        let index = state.index.as_ref().expect("index initialized");
        let matches = rank_matches(
            index,
            query,
            &query_embedding,
            path_filter.as_deref(),
            max_results,
        );

        Ok(SemanticSearchResult {
            query: query.to_string(),
            matches,
            indexed_files: index.files.len(),
            indexed_chunks: index.chunks.len(),
            updated_files,
            model: MODEL_ID.to_string(),
        })
    }

    fn index_path(&self) -> PathBuf {
        self.workspace.join(".medusa/semantic/index.bin")
    }

    fn embedding_server(&self, cancel: &CancelToken) -> Result<Arc<EmbeddingServer>> {
        let mut server = self
            .server
            .lock()
            .map_err(|_| eyre!("embedding server lock was poisoned"))?;
        if let Some(server) = server.as_ref() {
            return Ok(Arc::clone(server));
        }
        let started = Arc::new(EmbeddingServer::start(cancel)?);
        *server = Some(Arc::clone(&started));
        Ok(started)
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct StoredIndex {
    version: u32,
    model: String,
    dimensions: usize,
    files: BTreeMap<String, FileFingerprint>,
    chunks: Vec<StoredChunk>,
}

impl Default for StoredIndex {
    fn default() -> Self {
        Self {
            version: INDEX_VERSION,
            model: MODEL_ID.to_string(),
            dimensions: 0,
            files: BTreeMap::new(),
            chunks: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
struct FileFingerprint {
    size: u64,
    modified_ns: u128,
}

#[derive(Debug, Serialize, Deserialize)]
struct StoredChunk {
    path: String,
    start_line: usize,
    end_line: usize,
    preview: String,
    lexical: Vec<u64>,
    vector: Vec<i8>,
    scale: f32,
}

#[derive(Debug)]
struct SourceChunk {
    path: String,
    start_line: usize,
    end_line: usize,
    text: String,
    preview: String,
    lexical: Vec<u64>,
}

fn refresh_index(
    index: &mut StoredIndex,
    workspace: &Path,
    server: &EmbeddingServer,
    cancel: &CancelToken,
) -> Result<usize> {
    if index.version != INDEX_VERSION || index.model != MODEL_ID {
        *index = StoredIndex::default();
    }

    let snapshot = scan_workspace(workspace, cancel)?;
    let current_paths = snapshot.keys().cloned().collect::<BTreeSet<_>>();
    let removed = index
        .files
        .keys()
        .filter(|path| !current_paths.contains(*path))
        .cloned()
        .collect::<HashSet<_>>();
    let changed = snapshot
        .iter()
        .filter(|(path, (_, fingerprint))| index.files.get(*path) != Some(fingerprint))
        .map(|(path, _)| path.clone())
        .collect::<HashSet<_>>();

    if removed.is_empty() && changed.is_empty() {
        return Ok(0);
    }

    index
        .chunks
        .retain(|chunk| !removed.contains(&chunk.path) && !changed.contains(&chunk.path));
    for path in &removed {
        index.files.remove(path);
    }

    let mut pending = Vec::new();
    for path in &changed {
        cancel.bail_if_cancelled()?;
        let Some((absolute, fingerprint)) = snapshot.get(path) else {
            continue;
        };
        let Ok(content) = fs::read_to_string(absolute) else {
            continue;
        };
        pending.extend(chunk_source(path, &content));
        index.files.insert(path.clone(), *fingerprint);
    }

    let batch_size = env::var("MEDUSA_SEMANTIC_BATCH_SIZE")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(24)
        .clamp(1, 128);
    for batch in pending.chunks(batch_size) {
        cancel.bail_if_cancelled()?;
        let inputs = batch
            .iter()
            .map(|chunk| {
                format!(
                    "search_document: file: {}\nlines: {}-{}\n{}",
                    chunk.path, chunk.start_line, chunk.end_line, chunk.text
                )
            })
            .collect::<Vec<_>>();
        let vectors = server.embed(&inputs, cancel)?;
        if vectors.len() != batch.len() {
            bail!(
                "embedding server returned {} vectors for {} chunks",
                vectors.len(),
                batch.len()
            );
        }
        for (chunk, vector) in batch.iter().zip(vectors) {
            let (vector, scale) = quantize(&vector);
            index.dimensions = vector.len();
            index.chunks.push(StoredChunk {
                path: chunk.path.clone(),
                start_line: chunk.start_line,
                end_line: chunk.end_line,
                preview: chunk.preview.clone(),
                lexical: chunk.lexical.clone(),
                vector,
                scale,
            });
        }
    }

    index.chunks.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then_with(|| a.start_line.cmp(&b.start_line))
    });
    Ok(changed.len() + removed.len())
}

fn scan_workspace(
    workspace: &Path,
    cancel: &CancelToken,
) -> Result<BTreeMap<String, (PathBuf, FileFingerprint)>> {
    let mut found = BTreeMap::new();
    let mut pending = vec![workspace.to_path_buf()];
    while let Some(directory) = pending.pop() {
        cancel.bail_if_cancelled()?;
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                if !skip_directory(&name.to_string_lossy()) {
                    pending.push(path);
                }
                continue;
            }
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES {
                continue;
            }
            let relative = path
                .strip_prefix(workspace)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            if skip_file(&relative) {
                continue;
            }
            found.insert(
                relative,
                (
                    path,
                    FileFingerprint {
                        size: metadata.len(),
                        modified_ns: metadata
                            .modified()
                            .ok()
                            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                            .map_or(0, |duration| duration.as_nanos()),
                    },
                ),
            );
        }
    }
    Ok(found)
}

fn skip_directory(name: &str) -> bool {
    matches!(
        name,
        ".git"
            | ".medusa"
            | "target"
            | "node_modules"
            | ".next"
            | ".build"
            | "dist"
            | "build"
            | "vendor"
            | ".venv"
            | "venv"
            | "__pycache__"
    )
}

fn skip_file(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    [
        ".png", ".jpg", ".jpeg", ".gif", ".webp", ".ico", ".pdf", ".zip", ".gz", ".tar", ".woff",
        ".woff2", ".ttf", ".otf", ".mp3", ".mp4", ".mov", ".lock",
    ]
    .iter()
    .any(|extension| lower.ends_with(extension))
}

fn chunk_source(path: &str, content: &str) -> Vec<SourceChunk> {
    if content.contains('\0') || content.trim().is_empty() {
        return Vec::new();
    }
    let lines = content.lines().collect::<Vec<_>>();
    let mut chunks = Vec::new();
    let mut start = 0usize;
    while start < lines.len() {
        let mut end = start;
        let mut chars = 0usize;
        while end < lines.len() && end - start < CHUNK_MAX_LINES {
            chars += lines[end].chars().count() + 1;
            end += 1;
            if chars >= CHUNK_TARGET_CHARS {
                break;
            }
        }
        let text = lines[start..end].join("\n");
        let preview = text
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(300)
            .collect::<String>();
        chunks.push(SourceChunk {
            path: path.to_string(),
            start_line: start + 1,
            end_line: end,
            lexical: lexical_hashes(&format!("{path} {text}")),
            text,
            preview,
        });
        if end == lines.len() {
            break;
        }
        start = end.saturating_sub(CHUNK_OVERLAP_LINES).max(start + 1);
    }
    chunks
}

fn rank_matches(
    index: &StoredIndex,
    query: &str,
    query_embedding: &[f32],
    path_filter: Option<&str>,
    max_results: usize,
) -> Vec<SemanticMatch> {
    let query_tokens = lexical_hashes(query).into_iter().collect::<HashSet<_>>();
    let query_lower = query.to_ascii_lowercase();
    let mut scored = index
        .chunks
        .iter()
        .filter(|chunk| path_filter.is_none_or(|prefix| path_matches(&chunk.path, prefix)))
        .map(|chunk| {
            let semantic = dot_quantized(query_embedding, &chunk.vector, chunk.scale);
            let lexical_hits = chunk
                .lexical
                .iter()
                .filter(|token| query_tokens.contains(token))
                .count();
            let lexical = if query_tokens.is_empty() {
                0.0
            } else {
                lexical_hits as f32 / query_tokens.len() as f32
            };
            let path_bonus = query_lower
                .split(|ch: char| !ch.is_alphanumeric() && ch != '_' && ch != '-')
                .filter(|token| token.len() > 2)
                .any(|token| chunk.path.to_ascii_lowercase().contains(token))
                as u8 as f32;
            (semantic * 0.86 + lexical * 0.10 + path_bonus * 0.04, chunk)
        })
        .collect::<Vec<_>>();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));

    let mut seen = HashSet::new();
    scored
        .into_iter()
        .filter(|(_, chunk)| seen.insert(chunk.path.clone()))
        .take(max_results)
        .map(|(score, chunk)| SemanticMatch {
            path: chunk.path.clone(),
            start_line: chunk.start_line,
            end_line: chunk.end_line,
            score,
            preview: chunk.preview.clone(),
        })
        .collect()
}

fn path_matches(path: &str, filter: &str) -> bool {
    filter.is_empty() || path == filter || path.starts_with(&format!("{filter}/"))
}

fn normalize_filter_path(path: &Path) -> Result<String> {
    if path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        bail!("semantic_search.path must stay inside the workspace");
    }
    Ok(path.to_string_lossy().trim_matches('/').replace('\\', "/"))
}

fn lexical_hashes(text: &str) -> Vec<u64> {
    let mut tokens = text
        .split(|ch: char| !ch.is_alphanumeric() && ch != '_' && ch != '-')
        .map(str::trim)
        .filter(|token| token.len() > 1)
        .map(|token| stable_hash(&token.to_ascii_lowercase()))
        .collect::<Vec<_>>();
    tokens.sort_unstable();
    tokens.dedup();
    tokens.truncate(96);
    tokens
}

fn stable_hash(value: &str) -> u64 {
    value.bytes().fold(0xcbf29ce484222325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
    })
}

fn normalize(mut vector: Vec<f32>) -> Vec<f32> {
    let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
    if norm > f32::EPSILON {
        for value in &mut vector {
            *value /= norm;
        }
    }
    vector
}

fn quantize(vector: &[f32]) -> (Vec<i8>, f32) {
    let normalized = normalize(vector.to_vec());
    let max = normalized
        .iter()
        .map(|value| value.abs())
        .fold(0.0f32, f32::max);
    let scale = if max > f32::EPSILON { max / 127.0 } else { 1.0 };
    let quantized = normalized
        .iter()
        .map(|value| (value / scale).round().clamp(-127.0, 127.0) as i8)
        .collect();
    (quantized, scale)
}

fn dot_quantized(query: &[f32], vector: &[i8], scale: f32) -> f32 {
    query
        .iter()
        .zip(vector)
        .map(|(left, right)| left * f32::from(*right) * scale)
        .sum()
}

fn load_index(path: &Path) -> Result<StoredIndex> {
    if !path.exists() {
        return Ok(StoredIndex::default());
    }
    let bytes = fs::read(path).wrap_err_with(|| format!("failed to read {}", path.display()))?;
    let index: StoredIndex = bincode::deserialize(&bytes)
        .wrap_err_with(|| format!("failed to decode {}", path.display()))?;
    if index.version != INDEX_VERSION || index.model != MODEL_ID {
        return Ok(StoredIndex::default());
    }
    Ok(index)
}

fn save_index(path: &Path, index: &StoredIndex) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| eyre!("semantic index has no parent"))?;
    fs::create_dir_all(parent)
        .wrap_err_with(|| format!("failed to create {}", parent.display()))?;
    let bytes = bincode::serialize(index).wrap_err("failed to encode semantic index")?;
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    fs::write(&temporary, bytes)
        .wrap_err_with(|| format!("failed to write {}", temporary.display()))?;
    fs::rename(&temporary, path).wrap_err_with(|| format!("failed to install {}", path.display()))
}

#[derive(Debug)]
struct EmbeddingServer {
    endpoint: String,
    client: Client,
    child: Mutex<Child>,
}

impl EmbeddingServer {
    fn start(cancel: &CancelToken) -> Result<Self> {
        let model = ensure_model(cancel)?;
        let executable = env::var_os("MEDUSA_LLAMA_SERVER")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("llama-server"));
        let listener = TcpListener::bind("127.0.0.1:0")
            .wrap_err("failed to reserve a local embedding server port")?;
        let port = listener.local_addr()?.port();
        drop(listener);

        let child = Command::new(&executable)
            .args([
                "--model",
                &model.to_string_lossy(),
                "--embedding",
                "--pooling",
                "mean",
                "--host",
                "127.0.0.1",
                "--port",
                &port.to_string(),
                "--ctx-size",
                "8192",
                "--batch-size",
                "8192",
                "--ubatch-size",
                "1024",
                "--n-gpu-layers",
                "99",
                "--rope-scaling",
                "yarn",
                "--rope-freq-scale",
                "0.75",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .wrap_err_with(|| {
                format!(
                    "failed to start {}. Install llama.cpp (`brew install llama.cpp`) or set MEDUSA_LLAMA_SERVER",
                    executable.display()
                )
            })?;
        let client = Client::builder()
            .timeout(Duration::from_secs(300))
            .build()
            .wrap_err("failed to build embedding HTTP client")?;
        let endpoint = format!("http://127.0.0.1:{port}");
        for _ in 0..300 {
            cancel.bail_if_cancelled()?;
            if client
                .get(format!("{endpoint}/health"))
                .send()
                .is_ok_and(|response| response.status().is_success())
            {
                return Ok(Self {
                    endpoint,
                    client,
                    child: Mutex::new(child),
                });
            }
            thread::sleep(Duration::from_millis(100));
        }
        let mut child = child;
        let _ = child.kill();
        bail!("local embedding server did not become ready within 30 seconds")
    }

    fn embed(&self, input: &[String], cancel: &CancelToken) -> Result<Vec<Vec<f32>>> {
        cancel.bail_if_cancelled()?;
        let response = self
            .client
            .post(format!("{}/v1/embeddings", self.endpoint))
            .json(&json!({ "input": input, "model": MODEL_ID }))
            .send()
            .wrap_err("local embedding request failed")?;
        let status = response.status();
        let value: Value = response
            .json()
            .wrap_err("local embedding server returned invalid JSON")?;
        if !status.is_success() {
            bail!("local embedding server returned {status}: {value}");
        }
        let data = value
            .get("data")
            .and_then(Value::as_array)
            .ok_or_else(|| eyre!("embedding response did not contain data"))?;
        data.iter()
            .map(|item| {
                let values = item
                    .get("embedding")
                    .and_then(Value::as_array)
                    .ok_or_else(|| eyre!("embedding response item was malformed"))?;
                values
                    .iter()
                    .map(|value| {
                        value
                            .as_f64()
                            .map(|value| value as f32)
                            .ok_or_else(|| eyre!("embedding contained a non-number"))
                    })
                    .collect()
            })
            .collect()
    }
}

impl Drop for EmbeddingServer {
    fn drop(&mut self) {
        if let Ok(child) = self.child.get_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn ensure_model(cancel: &CancelToken) -> Result<PathBuf> {
    if let Some(path) = env::var_os("MEDUSA_SEMANTIC_MODEL") {
        let path = PathBuf::from(path);
        if !path.is_file() {
            bail!("MEDUSA_SEMANTIC_MODEL does not exist: {}", path.display());
        }
        return Ok(path);
    }
    let path = data_home().join("models").join(MODEL_NAME);
    if path.is_file()
        && path
            .metadata()
            .is_ok_and(|metadata| metadata.len() > 1_000_000)
    {
        return Ok(path);
    }
    let parent = path
        .parent()
        .ok_or_else(|| eyre!("model path has no parent"))?;
    fs::create_dir_all(parent)
        .wrap_err_with(|| format!("failed to create {}", parent.display()))?;
    let temporary = path.with_extension(format!("download-{}", std::process::id()));
    let url = env::var("MEDUSA_SEMANTIC_MODEL_URL").unwrap_or_else(|_| MODEL_URL.to_string());
    cancel.bail_if_cancelled()?;
    let mut response = Client::builder()
        .timeout(Duration::from_secs(1_800))
        .build()?
        .get(&url)
        .send()
        .wrap_err_with(|| format!("failed to download semantic model from {url}"))?
        .error_for_status()
        .wrap_err_with(|| format!("semantic model download failed: {url}"))?;
    let mut file = fs::File::create(&temporary)
        .wrap_err_with(|| format!("failed to create {}", temporary.display()))?;
    response
        .copy_to(&mut file)
        .wrap_err("semantic model download was interrupted")?;
    file.flush()?;
    if file.metadata()?.len() < 1_000_000 {
        let _ = fs::remove_file(&temporary);
        bail!("downloaded semantic model is unexpectedly small");
    }
    fs::rename(&temporary, &path)
        .wrap_err_with(|| format!("failed to install model at {}", path.display()))?;
    Ok(path)
}

fn data_home() -> PathBuf {
    env::var_os("MEDUSA_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("XDG_DATA_HOME").map(|path| PathBuf::from(path).join("medusa")))
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share/medusa")))
        .unwrap_or_else(|| PathBuf::from(".medusa-data"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_overlap_and_keep_line_ranges() {
        let content = (1..=300)
            .map(|line| format!("fn line_{line}() {{}}"))
            .collect::<Vec<_>>()
            .join("\n");
        let chunks = chunk_source("src/lib.rs", &content);
        assert!(chunks.len() >= 3);
        assert_eq!(chunks[0].start_line, 1);
        assert!(chunks[1].start_line <= chunks[0].end_line);
        assert_eq!(chunks.last().unwrap().end_line, 300);
    }

    #[test]
    fn quantized_vectors_preserve_similarity() {
        let vector = vec![0.5, -0.25, 0.75, 0.1];
        let normalized = normalize(vector.clone());
        let (quantized, scale) = quantize(&vector);
        assert!(dot_quantized(&normalized, &quantized, scale) > 0.99);
    }

    #[test]
    fn ranking_deduplicates_files() {
        let query = normalize(vec![1.0, 0.0]);
        let chunk = |path: &str, line, vector: Vec<i8>| StoredChunk {
            path: path.to_string(),
            start_line: line,
            end_line: line + 5,
            preview: path.to_string(),
            lexical: Vec::new(),
            vector,
            scale: 1.0 / 127.0,
        };
        let index = StoredIndex {
            dimensions: 2,
            chunks: vec![
                chunk("a.rs", 1, vec![127, 0]),
                chunk("a.rs", 20, vec![126, 0]),
                chunk("b.rs", 1, vec![100, 10]),
            ],
            ..StoredIndex::default()
        };
        let matches = rank_matches(&index, "handler", &query, None, 5);
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].path, "a.rs");
        assert_eq!(matches[1].path, "b.rs");
    }

    #[cfg(unix)]
    #[test]
    fn workspace_scan_never_follows_symlinks() {
        use std::os::unix::fs::symlink;

        let root = env::temp_dir().join(format!("medusa-semantic-symlink-{}", std::process::id()));
        let outside =
            env::temp_dir().join(format!("medusa-semantic-outside-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&outside);
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(root.join("inside.rs"), "fn inside() {}\n").unwrap();
        fs::write(outside.join("secret.rs"), "fn secret() {}\n").unwrap();
        symlink(&outside, root.join("linked")).unwrap();

        let files = scan_workspace(&root, &CancelToken::default()).unwrap();
        assert!(files.contains_key("inside.rs"));
        assert!(!files.keys().any(|path| path.contains("secret")));
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(outside);
    }

    #[test]
    #[ignore = "downloads the Nomic model and starts the local llama.cpp server"]
    fn live_local_semantic_search_smoke() {
        let workspace =
            env::temp_dir().join(format!("medusa-semantic-smoke-{}", std::process::id()));
        let _ = fs::remove_dir_all(&workspace);
        fs::create_dir_all(workspace.join("src")).unwrap();
        fs::write(
            workspace.join("src/auth.rs"),
            "pub fn refresh_expired_oauth_token() { /* rotate credentials */ }\n",
        )
        .unwrap();
        fs::write(
            workspace.join("src/render.rs"),
            "pub fn draw_terminal_frame() { /* paint widgets */ }\n",
        )
        .unwrap();

        let runtime = SemanticRuntime::new(workspace.clone());
        let result = runtime
            .search(
                SemanticSearchRequest {
                    query: "renew login credentials when the session expires".to_string(),
                    path: None,
                    max_results: Some(2),
                },
                &CancelToken::default(),
            )
            .unwrap();
        assert_eq!(result.matches[0].path, "src/auth.rs");
        assert_eq!(result.indexed_files, 2);
        assert_eq!(result.updated_files, 2);

        let unchanged = runtime
            .search(
                SemanticSearchRequest {
                    query: "draw the terminal interface".to_string(),
                    path: None,
                    max_results: Some(2),
                },
                &CancelToken::default(),
            )
            .unwrap();
        assert_eq!(unchanged.updated_files, 0);
        assert_eq!(unchanged.matches[0].path, "src/render.rs");

        fs::write(
            workspace.join("src/render.rs"),
            "pub fn draw_terminal_frame() { /* paint widgets and status bar */ }\n",
        )
        .unwrap();
        let refreshed = runtime
            .search(
                SemanticSearchRequest {
                    query: "paint the status bar".to_string(),
                    path: Some(PathBuf::from("src")),
                    max_results: Some(2),
                },
                &CancelToken::default(),
            )
            .unwrap();
        assert_eq!(refreshed.updated_files, 1);
        assert_eq!(refreshed.matches[0].path, "src/render.rs");
        let _ = fs::remove_dir_all(workspace);
    }
}
