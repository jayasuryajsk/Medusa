//! Interactive TUI application state and event loop.

pub(super) use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    env, fs, io,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc,
        mpsc::{self, Receiver, Sender, TryRecvError},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub(super) use arboard::Clipboard;
pub(super) use color_eyre::eyre::{Result, WrapErr};
pub(super) use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
pub(super) use image::{ColorType, ImageEncoder, codecs::png::PngEncoder};
pub(super) use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Margin, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Cell, Clear, LineGauge, List, ListItem, ListState, Padding,
        Paragraph, Row, Scrollbar, ScrollbarOrientation, ScrollbarState, Table, Wrap,
    },
};

pub(super) use medusa_core::agents::AgentRegistry;
pub(super) use medusa_core::cancel::{CancelToken, error_is_cancellation};
pub(super) use medusa_core::checkpoint::{
    CheckpointEntry, CheckpointMeta, CheckpointRecorder, CheckpointStore, CheckpointSummary,
    RetentionLimits,
};
pub(super) use medusa_core::context::{ContextEngine, ManualCompaction};
pub(super) use medusa_core::mcp::{McpRegistry, McpServerStateLabel, McpServerStatus};
pub(super) use medusa_core::model::{
    ConversationMessage, ModelGateway, ModelStreamEvent, TokenUsage,
};
pub(super) use medusa_core::permissions::PermissionMode;
pub(super) use medusa_core::persistence::atomic_write_private;
pub(super) use medusa_core::session::{
    SessionOpenMode, SessionStore as CoreSessionStore, compact_session_id, human_bytes,
};
pub(super) use medusa_core::tools::{
    ApprovalDecision, ApprovalRequest, ApprovalTool, BackgroundJobEvent, FilePatchRequest,
    TerminalExecRequest, TerminalExecResult, ToolRuntime,
};
pub(super) use medusa_core::workflow::{
    WorkflowEvent, WorkflowRuntime, WorkflowScript, WorkflowStatus,
};

pub(super) use crate::config::*;
pub(super) use crate::render::*;
pub(super) use crate::session_memory::*;
pub(super) use crate::slash::*;
pub(super) use crate::styles::*;
pub(super) use crate::terminal::Tui;
#[cfg(test)]
pub(super) use crate::terminal::maybe_rebuild_before_reload;
pub(super) use crate::types::*;
pub(super) use crate::util::*;

pub(crate) type SessionStore = CoreSessionStore<TranscriptItem>;

pub(super) use crate::constants::*;

pub(crate) struct App {
    input: String,
    input_cursor: usize,
    pending_attachments: Vec<ImageAttachment>,
    attachment_previews: HashMap<String, Vec<Line<'static>>>,
    image_renderer: TerminalImageRenderer,
    transcript: Vec<TranscriptItem>,
    transcript_version: u64,
    transcript_rows_cache: Option<TranscriptRowsCache>,
    status_line: String,
    last_chat_viewport: Option<Rect>,
    last_transcript_rows: Arc<Vec<TranscriptRow>>,
    should_quit: bool,
    pub(crate) restart_requested: bool,
    cwd_display: String,
    inside_git_repo: bool,
    theme: ThemeKind,
    permission_mode: PermissionMode,
    tools: ToolRuntime,
    /// App-owned MCP registry, re-injected into every ToolRuntime rebuild so
    /// permission-mode switches never respawn live MCP servers.
    pub(crate) mcp: Arc<McpRegistry>,
    /// Snapshot rendered by the /mcp modal, captured when the command runs so
    /// drawing never blocks on a connecting server's state lock.
    mcp_statuses: Vec<McpServerStatus>,
    /// Snapshot rendered by the /agents modal, reloaded from .medusa/agents
    /// each time the command runs so file edits show up without a restart.
    agent_registry: AgentRegistry,
    model: ModelGateway,
    context_engine: ContextEngine,
    plan_mode: bool,
    model_enabled: bool,
    model_events: Option<Receiver<ModelStreamEvent>>,
    workflow_events: Vec<BackgroundWorkflow>,
    background_job_sender: Sender<BackgroundJobEvent>,
    approval_handler: medusa_core::tools::ApprovalHandler,
    approval_events: Receiver<PendingApproval>,
    approval_queue: VecDeque<PendingApproval>,
    session_terminal_grants: Vec<String>,
    session_edit_grants: Vec<String>,
    /// Set once the user picks "always allow" on a web egress prompt; every
    /// later web_fetch/web_search then auto-allows for the rest of the session.
    session_web_egress_allowed: bool,
    denied_this_turn: Vec<String>,
    approval_shown_at: Option<Instant>,
    denied_edits_this_turn: Vec<String>,
    background_job_events: Receiver<BackgroundJobEvent>,
    background_jobs: BTreeMap<String, BackgroundJobView>,
    streaming_message: Option<usize>,
    queued_turns: VecDeque<String>,
    /// Cancel token for the streaming turn; None while idle.
    turn_cancel: Option<CancelToken>,
    /// Set on the first Esc while working; a second Esc force-abandons.
    cancel_requested_at: Option<Instant>,
    last_stream_save: Instant,
    chat_scroll: usize,
    chat_scroll_target: usize,
    selected_tool: Option<usize>,
    decision_selection: usize,
    workflows: Vec<WorkflowRunView>,
    animation_tick: u64,
    started_at: Instant,
    turn_started_at: Option<Instant>,
    last_escape_at: Option<Instant>,
    session: Option<SessionStore>,
    active_modal: Option<Modal>,
    slash_selection: usize,
    mention_selection: usize,
    /// Workspace file list backing the @ mention picker; loaded when a
    /// mention token appears and dropped when it goes away, so every picker
    /// activation sees fresh files without re-walking per keystroke.
    mention_files: Option<Vec<String>>,
    /// Esc closed the picker for the current @token; the next edit reopens.
    mention_dismissed: bool,
    /// Workspace bell preference (MEDUSA_BELL can override at ring time).
    bell_setting: bool,
    settings_selection: usize,
    model_selection: usize,
    reasoning_selection: usize,
    model_picker_pane: ModelPickerPane,
    permission_selection: usize,
    theme_selection: usize,
    image_preview_index: usize,
    image_preview_zoom: u16,
    theme_preview_original: Option<ThemeKind>,
    toast: Option<Toast>,
    /// Backend-reported token usage summed over every request this app run.
    session_usage: TokenUsage,
    session_requests: usize,
    /// Usage accumulated across the streaming turn's requests (one model
    /// request per tool iteration); reset when a new turn starts.
    turn_usage: TokenUsage,
    turn_requests: usize,
    /// Usage of the most recently finished turn, for the /cost readout.
    last_turn_usage: TokenUsage,
    last_turn_requests: usize,
    /// Snapshot rendered by the /context modal, captured when the command ran.
    context_report: Option<ContextReport>,
    /// Result channel for a background /compact run; None while idle.
    compact_events: Option<Receiver<Result<ManualCompaction, String>>>,
    /// Recorder for the turn currently streaming; finished on turn end.
    active_checkpoint: Option<CheckpointRecorder>,
    /// Test-only capture of the exact `ToolRuntime` handed to the last model
    /// turn's worker, so tests can assert the checkpoint recorder and cancel
    /// token were actually wired onto it (not just onto `App`).
    #[cfg(test)]
    last_turn_runtime: Option<ToolRuntime>,
    /// Test-only capture of the `ToolRuntime` handed to the last background
    /// workflow's worker, for the same wiring assertions.
    #[cfg(test)]
    last_workflow_runtime: Option<ToolRuntime>,
    rewind_entries: Vec<CheckpointEntry>,
    rewind_selection: usize,
    rewind_stage: RewindStage,
    rewind_confirm_selection: usize,
    /// Rows offered by the /edit backtrack picker: previous user messages,
    /// newest first.
    edit_picker_entries: Vec<EditPickerEntry>,
    edit_picker_selection: usize,
    /// Git probe used by /review; a fn pointer so tests can exercise both
    /// the seeded and the "nothing to review" paths without a real repo.
    review_diff_check: fn(&Path) -> bool,
}

mod approvals;
mod attachments;
mod commands;
mod composer;
mod draw;
mod draw_config;
mod draw_overlays;
mod draw_sessions;
mod input;
mod job_actions;
mod jobs;
mod lifecycle;
mod scroll;
mod session_ops;
mod tools_ui;
mod turn;

#[cfg(test)]
mod tests;
