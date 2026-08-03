use std::{
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
};

use color_eyre::eyre::Result;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalExecRequest {
    pub command: String,
    pub cwd: Option<PathBuf>,
    pub background: bool,
    /// Model-requested sandbox escalation (`"sandbox": false`). Always routes
    /// through user approval; refused outright in readonly mode.
    pub unsandboxed: bool,
}

impl TerminalExecRequest {
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            cwd: None,
            background: false,
            unsandboxed: false,
        }
    }

    pub fn background(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            cwd: None,
            background: true,
            unsandboxed: false,
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
    /// Whether the command actually ran inside the Seatbelt sandbox.
    pub sandboxed: bool,
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

pub(crate) fn background_job_id(pid: u32, command: &str, cwd: &Path) -> String {
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
    pub include: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSearchResult {
    pub query: String,
    pub regex: bool,
    pub matches: Vec<SearchMatch>,
    pub searched_files: usize,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticSearchRequest {
    pub query: String,
    pub path: Option<PathBuf>,
    pub max_results: Option<usize>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SemanticSearchResult {
    pub query: String,
    pub matches: Vec<SemanticMatch>,
    pub indexed_files: usize,
    pub indexed_chunks: usize,
    pub updated_files: usize,
    pub model: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SemanticMatch {
    pub path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub score: f32,
    pub preview: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileGlobRequest {
    pub pattern: String,
    pub path: Option<PathBuf>,
    pub max_results: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileGlobResult {
    pub pattern: String,
    pub root: String,
    pub paths: Vec<String>,
    pub truncated: bool,
}

pub(crate) enum SearchMatcher {
    Regex(regex::Regex),
    Literal {
        needle: String,
        case_sensitive: bool,
    },
}

impl SearchMatcher {
    pub(crate) fn new(query: &str, case_sensitive: bool) -> Self {
        match regex::RegexBuilder::new(query)
            .case_insensitive(!case_sensitive)
            .size_limit(1 << 20)
            .build()
        {
            Ok(regex) => Self::Regex(regex),
            Err(_) => Self::Literal {
                needle: if case_sensitive {
                    query.to_string()
                } else {
                    query.to_ascii_lowercase()
                },
                case_sensitive,
            },
        }
    }

    pub(crate) fn is_regex(&self) -> bool {
        matches!(self, Self::Regex(_))
    }

    pub(crate) fn is_match(&self, line: &str) -> bool {
        match self {
            Self::Regex(regex) => regex.is_match(line),
            Self::Literal {
                needle,
                case_sensitive,
            } => {
                if *case_sensitive {
                    line.contains(needle)
                } else {
                    line.to_ascii_lowercase().contains(needle)
                }
            }
        }
    }
}

pub(crate) fn compile_include_glob(pattern: &str) -> Result<globset::GlobMatcher> {
    let pattern = pattern.trim();
    // Bare-name patterns like "*.rs" should match at any directory depth.
    let expanded = if pattern.contains('/') {
        pattern.to_string()
    } else {
        format!("**/{pattern}")
    };
    Ok(globset::GlobBuilder::new(&expanded)
        .literal_separator(true)
        .build()
        .map_err(|error| color_eyre::eyre::eyre!("invalid glob pattern {pattern:?}: {error}"))?
        .compile_matcher())
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
