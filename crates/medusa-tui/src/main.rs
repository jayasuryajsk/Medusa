use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    env, fs, io,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU8, Ordering},
        mpsc::{self, Receiver, Sender, TryRecvError},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use arboard::Clipboard;
use color_eyre::eyre::{Result, WrapErr, bail};
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use image::{ColorType, ImageEncoder, codecs::png::PngEncoder};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Cell, Clear, LineGauge, List, ListItem, ListState, Padding,
        Paragraph, Row, Scrollbar, ScrollbarOrientation, ScrollbarState, Table, Wrap,
    },
};
use ratatui_image::{
    Resize,
    picker::Picker,
    sliced::{SignedPosition, SlicedImage, SlicedProtocol},
};
use serde::{Deserialize, Serialize};

use medusa_core::agents::AgentRegistry;
use medusa_core::auth::probe_codex_auth;
use medusa_core::cancel::{CancelToken, error_is_cancellation};
use medusa_core::checkpoint::{
    CheckpointEntry, CheckpointMeta, CheckpointRecorder, CheckpointStore, CheckpointSummary,
    RetentionLimits,
};
use medusa_core::context::{ContextEngine, ManualCompaction};
use medusa_core::mcp::{McpRegistry, McpServerStateLabel, McpServerStatus};
use medusa_core::model::{
    ConversationAttachment, ConversationMessage, DirectCodexBackend, ModelStreamEvent, TokenUsage,
};
use medusa_core::permissions::{PermissionMode, PermissionPolicy};
use medusa_core::session::{
    SessionOpenMode, SessionStore as CoreSessionStore, compact_session_id, human_bytes,
};
use medusa_core::tools::{
    ApprovalDecision, ApprovalRequest, ApprovalTool, BackgroundJobEvent, FilePatchRequest,
    TerminalExecRequest, TerminalExecResult, ToolRuntime,
};
use medusa_core::workflow::{
    SubagentToolPolicy, WorkflowEvent, WorkflowPhasePlan, WorkflowRuntime, WorkflowScript,
    WorkflowStatus,
};

mod animation;
mod terminal;

#[cfg(test)]
use terminal::maybe_rebuild_before_reload;
use terminal::{Tui, init_terminal, relaunch_current_executable, restore_terminal};

type SessionStore = CoreSessionStore<TranscriptItem>;

fn main() -> Result<()> {
    color_eyre::install()?;

    let startup_command = parse_args()?;
    if let StartupCommand::Headless(options) = startup_command {
        return run_headless(options);
    }

    let StartupCommand::Tui(startup_session) = startup_command else {
        unreachable!("headless command returned above");
    };

    let mut terminal = init_terminal()?;
    let mut app = App::new(startup_session)?;
    let app_result = app.run(&mut terminal);
    let restart_requested = app.restart_requested;
    restore_terminal(&mut terminal)?;
    // Reap MCP server children deterministically (stdin EOF, then a bounded
    // kill) instead of leaning on process exit.
    app.mcp.shutdown();

    app_result?;

    if restart_requested {
        relaunch_current_executable()?;
    }

    Ok(())
}

#[derive(Debug)]
struct BackgroundJobView {
    id: String,
    pid: u32,
    command: String,
    cwd: PathBuf,
    state: ToolRunState,
    started_at: Instant,
    finished_at: Option<Instant>,
    exit_code: Option<i32>,
    last_output: String,
}

#[derive(Debug, Clone)]
struct SettingsItem {
    key: &'static str,
    value: String,
    description: &'static str,
    action: &'static str,
    editable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum StartupCommand {
    Tui(SessionOpenMode),
    Headless(HeadlessOptions),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HeadlessOptions {
    task: Option<String>,
    model: Option<String>,
    permission_mode: Option<PermissionMode>,
    json: bool,
    stream: bool,
}

impl Default for HeadlessOptions {
    fn default() -> Self {
        Self {
            task: None,
            model: None,
            permission_mode: None,
            json: false,
            stream: true,
        }
    }
}

fn parse_args() -> Result<StartupCommand> {
    let args = env::args().skip(1).collect::<Vec<_>>();
    parse_startup_command(&args)
}

fn parse_startup_command(args: &[String]) -> Result<StartupCommand> {
    match args {
        [] => Ok(StartupCommand::Tui(SessionOpenMode::New)),
        [command] if command == "continue" => {
            Ok(StartupCommand::Tui(SessionOpenMode::ContinueLast))
        }
        [command, session] if command == "continue" => Ok(StartupCommand::Tui(
            SessionOpenMode::ContinueNamed(session.to_string()),
        )),
        [command, rest @ ..] if command == "run" => parse_headless_options(rest),
        [command] if command == "--help" || command == "-h" => {
            println!(
                "Usage: medusa [continue [session]]\n       medusa run [options] [--] <task>\n\nCommands:\n  continue            Resume the last Medusa TUI session in this workspace\n  continue <session>  Resume a specific session from .medusa/sessions\n  run                 Run one non-interactive headless agent turn\n\nRun options:\n  --model <name>                 Override the model for this run\n  --permission <open|guarded|readonly>\n  --json                         Print a machine-readable JSON result\n  --no-stream                    Print only the final answer\n\nIf <task> is omitted, medusa run reads the task from stdin."
            );
            std::process::exit(0);
        }
        [command] => bail!("unknown command `{command}` (try `medusa run` or `medusa continue`)"),
        _ => bail!(
            "too many arguments (usage: medusa [continue [session]] | medusa run [options] [--] <task>)"
        ),
    }
}

fn parse_headless_options(args: &[String]) -> Result<StartupCommand> {
    let mut options = HeadlessOptions::default();
    let mut task_parts = Vec::new();
    let mut index = 0usize;
    let mut passthrough = false;

    while index < args.len() {
        let arg = &args[index];
        if passthrough {
            task_parts.push(arg.clone());
            index += 1;
            continue;
        }

        match arg.as_str() {
            "--" => {
                passthrough = true;
                index += 1;
            }
            "--json" => {
                options.json = true;
                options.stream = false;
                index += 1;
            }
            "--no-stream" => {
                options.stream = false;
                index += 1;
            }
            "--stream" => {
                options.stream = true;
                index += 1;
            }
            "--model" | "-m" => {
                let Some(model) = args.get(index + 1) else {
                    bail!("{arg} requires a model name");
                };
                options.model = Some(model.clone());
                index += 2;
            }
            "--permission" | "--permissions" | "-p" => {
                let Some(mode) = args.get(index + 1) else {
                    bail!("{arg} requires open, guarded, or readonly");
                };
                options.permission_mode =
                    Some(PermissionMode::from_name(mode).ok_or_else(|| {
                        color_eyre::eyre::eyre!("unknown permission mode `{mode}`")
                    })?);
                index += 2;
            }
            "--help" | "-h" => {
                println!(
                    "Usage: medusa run [options] [--] <task>\n\nOptions:\n  --model <name>                 Override the model for this run\n  --permission <open|guarded|readonly>\n  --json                         Print a machine-readable JSON result\n  --no-stream                    Print only the final answer\n\nIf <task> is omitted, medusa run reads the task from stdin."
                );
                std::process::exit(0);
            }
            value if value.starts_with('-') && task_parts.is_empty() => {
                bail!("unknown medusa run option `{value}`");
            }
            value => {
                task_parts.push(value.to_string());
                index += 1;
            }
        }
    }

    if !task_parts.is_empty() {
        options.task = Some(task_parts.join(" "));
    }

    Ok(StartupCommand::Headless(options))
}

#[derive(Debug, Serialize)]
struct HeadlessToolEvent {
    name: String,
    summary: String,
    failed: Option<bool>,
}

#[derive(Debug, Serialize)]
struct HeadlessRunResult {
    success: bool,
    model: String,
    permission_mode: String,
    event_count: usize,
    answer: String,
    tools: Vec<HeadlessToolEvent>,
}

fn run_headless(options: HeadlessOptions) -> Result<()> {
    let cwd = env::current_dir().unwrap_or_else(|_| Path::new(".").to_path_buf());
    let tools = ToolRuntime::new(&cwd).wrap_err("failed to initialize Medusa tools")?;
    let tools = match McpRegistry::load(tools.workspace()) {
        Ok(registry) => tools.with_mcp(registry),
        Err(error) => {
            eprintln!("warning: MCP config ignored: {error}");
            tools
        }
    };
    let settings = load_app_settings(tools.workspace()).unwrap_or_default();
    let permission_mode = options
        .permission_mode
        .unwrap_or_else(|| settings.permission_mode());
    let mut backend =
        DirectCodexBackend::new(tools.workspace().to_path_buf()).wrap_err("HTTP client builds")?;

    let model_override = options.model.clone().or_else(|| {
        if env::var_os("MEDUSA_MODEL").is_none() {
            settings.model()
        } else {
            None
        }
    });
    if let Some(model) = model_override {
        backend.set_model_name(model);
    }

    let task = read_headless_task(&options)?;
    let prompt = headless_conversation_history(
        &task,
        tools.workspace(),
        backend.model_name(),
        permission_mode,
    );
    let mut answer = String::new();
    let mut tools_seen = Vec::new();
    let mut stdout = io::stdout();

    if !options.json {
        eprintln!(
            "medusa headless · model {} · permission {} · workspace {}",
            backend.model_name(),
            permission_mode.name(),
            abbreviate_home(&tools.workspace().to_string_lossy())
        );
    }

    let result = if permission_mode == PermissionMode::Readonly {
        backend.chat_stream_messages_read_only(&prompt, tools, |event| {
            handle_headless_event(event, &options, &mut answer, &mut tools_seen, &mut stdout)
        })
    } else {
        backend.chat_stream_messages(&prompt, tools, |event| {
            handle_headless_event(event, &options, &mut answer, &mut tools_seen, &mut stdout)
        })
    };

    match result {
        Ok(event_count) => {
            if options.stream && !options.json && !answer.ends_with('\n') {
                println!();
            } else if !options.stream && !options.json {
                println!("{}", answer.trim());
            }

            if options.json {
                let result = HeadlessRunResult {
                    success: true,
                    model: backend.model_name().to_string(),
                    permission_mode: permission_mode.name().to_string(),
                    event_count,
                    answer: answer.trim().to_string(),
                    tools: tools_seen,
                };
                println!("{}", serde_json::to_string_pretty(&result)?);
            }
            Ok(())
        }
        Err(error) => {
            let clean_error = clean_model_error(&error.to_string());
            if options.json {
                let result = HeadlessRunResult {
                    success: false,
                    model: backend.model_name().to_string(),
                    permission_mode: permission_mode.name().to_string(),
                    event_count: 0,
                    answer: clean_error,
                    tools: tools_seen,
                };
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                eprintln!("{clean_error}");
            }
            std::process::exit(1);
        }
    }
}

fn read_headless_task(options: &HeadlessOptions) -> Result<String> {
    if let Some(task) = options
        .task
        .as_deref()
        .map(str::trim)
        .filter(|task| !task.is_empty())
    {
        return Ok(task.to_string());
    }

    let mut task = String::new();
    io::stdin()
        .read_to_string(&mut task)
        .wrap_err("failed to read task from stdin")?;
    let task = task.trim().to_string();
    if task.is_empty() {
        bail!("medusa run requires a task argument or stdin input");
    }
    Ok(task)
}

fn headless_conversation_history(
    task: &str,
    workspace: &Path,
    model: &str,
    permission_mode: PermissionMode,
) -> Vec<ConversationMessage> {
    vec![
        ConversationMessage {
            role: "system".to_string(),
            content: permission_context_text(permission_mode).to_string(),
            attachments: Vec::new(),
        },
        ConversationMessage {
            role: "system".to_string(),
            content: format!(
                "Medusa headless run. Workspace: {}. Model: {model}. Run autonomously in this non-interactive CLI harness. Use tools to inspect, edit, and verify as needed. Return the final answer only after the task is complete or clearly blocked.",
                workspace.display()
            ),
            attachments: Vec::new(),
        },
        ConversationMessage {
            role: "user".to_string(),
            content: task.to_string(),
            attachments: Vec::new(),
        },
    ]
}

fn handle_headless_event(
    event: ModelStreamEvent,
    options: &HeadlessOptions,
    answer: &mut String,
    tools_seen: &mut Vec<HeadlessToolEvent>,
    stdout: &mut io::Stdout,
) -> Result<()> {
    match event {
        ModelStreamEvent::Delta(delta) => {
            answer.push_str(&delta);
            if options.stream && !options.json {
                print!("{delta}");
                stdout.flush().wrap_err("failed to flush stdout")?;
            }
        }
        ModelStreamEvent::ReasoningDelta(delta) => {
            if options.stream && !options.json {
                eprintln!("reasoning: {}", compact_one_line(&delta, 160));
            }
        }
        ModelStreamEvent::ToolStart { name, summary, .. } => {
            if !options.json {
                eprintln!("tool start: {name} · {}", compact_one_line(&summary, 180));
            }
            tools_seen.push(HeadlessToolEvent {
                name,
                summary,
                failed: None,
            });
        }
        ModelStreamEvent::ToolResult { name, output, .. } => {
            let failed = tool_output_failed(&output);
            let detail = compact_tool_detail(&output);
            if !options.json {
                let status = if failed { "failed" } else { "done" };
                eprintln!("tool {status}: {name} · {}", compact_one_line(&detail, 220));
            }
            tools_seen.push(HeadlessToolEvent {
                name,
                summary: detail,
                failed: Some(failed),
            });
        }
        ModelStreamEvent::Workflow(event) => {
            if !options.json {
                match &event {
                    WorkflowEvent::RunStarted { title, .. } => {
                        eprintln!("workflow started: {title}");
                    }
                    WorkflowEvent::PhaseStarted { name, .. } => {
                        eprintln!("workflow phase: {name}");
                    }
                    WorkflowEvent::AgentFinished { name, status, .. } => {
                        eprintln!("workflow agent {name}: {status:?}");
                    }
                    WorkflowEvent::Log { message, .. } => {
                        eprintln!("workflow log: {message}");
                    }
                    WorkflowEvent::RunFinished { status, .. } => {
                        eprintln!("workflow finished: {status:?}");
                    }
                    WorkflowEvent::AgentStarted { .. } | WorkflowEvent::PhaseFinished { .. } => {}
                }
            }
        }
        ModelStreamEvent::Usage(_)
        | ModelStreamEvent::Done { .. }
        | ModelStreamEvent::Error(_)
        | ModelStreamEvent::Cancelled => {}
    }
    Ok(())
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct AppSettings {
    #[serde(default)]
    theme: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    permission_mode: Option<String>,
    #[serde(default)]
    bell: Option<bool>,
    #[serde(default)]
    reasoning_effort: Option<String>,
}

impl AppSettings {
    fn theme(&self) -> Option<ThemeKind> {
        self.theme.as_deref().and_then(ThemeKind::from_name)
    }

    fn model(&self) -> Option<String> {
        self.model
            .as_deref()
            .map(str::trim)
            .filter(|model| !model.is_empty())
            .map(ToString::to_string)
    }

    fn reasoning_effort(&self) -> Option<String> {
        self.reasoning_effort
            .as_deref()
            .map(str::trim)
            .filter(|effort| !effort.is_empty())
            .map(ToString::to_string)
    }

    fn permission_mode(&self) -> PermissionMode {
        self.permission_mode
            .as_deref()
            .and_then(PermissionMode::from_name)
            .unwrap_or(PermissionMode::Open)
    }
}

fn app_settings_path(workspace: &Path) -> PathBuf {
    workspace.join(".medusa").join("settings.json")
}

fn load_app_settings(workspace: &Path) -> Result<AppSettings> {
    let path = app_settings_path(workspace);
    if !path.exists() {
        return Ok(AppSettings::default());
    }

    let text =
        fs::read_to_string(&path).wrap_err_with(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&text).wrap_err_with(|| format!("failed to parse {}", path.display()))
}

fn save_app_settings(workspace: &Path, settings: &AppSettings) -> Result<()> {
    let path = app_settings_path(workspace);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .wrap_err_with(|| format!("failed to create {}", parent.display()))?;
    }

    let json = serde_json::to_string_pretty(settings).wrap_err("failed to encode settings")?;
    fs::write(&path, json).wrap_err_with(|| format!("failed to write {}", path.display()))
}

fn save_theme_preference(workspace: &Path, theme: ThemeKind) -> Result<()> {
    let mut settings = load_app_settings(workspace).unwrap_or_default();
    settings.theme = Some(theme.name().to_string());
    save_app_settings(workspace, &settings)
}

fn save_model_preference(workspace: &Path, model: &str) -> Result<()> {
    let mut settings = load_app_settings(workspace).unwrap_or_default();
    settings.model = Some(model.trim().to_string());
    save_app_settings(workspace, &settings)
}

fn save_reasoning_preference(workspace: &Path, effort: &str) -> Result<()> {
    let mut settings = load_app_settings(workspace).unwrap_or_default();
    settings.reasoning_effort = Some(effort.trim().to_string());
    save_app_settings(workspace, &settings)
}

fn save_permission_mode_preference(workspace: &Path, mode: PermissionMode) -> Result<()> {
    let mut settings = load_app_settings(workspace).unwrap_or_default();
    settings.permission_mode = Some(mode.name().to_string());
    save_app_settings(workspace, &settings)?;
    PermissionPolicy::write_mode(workspace, mode)
}

fn save_bell_preference(workspace: &Path, enabled: bool) -> Result<()> {
    let mut settings = load_app_settings(workspace).unwrap_or_default();
    settings.bell = Some(enabled);
    save_app_settings(workspace, &settings)
}

/// Effective bell enablement: the MEDUSA_BELL environment variable overrides
/// the workspace setting ("off"/"0"/"false"/"no" disables, "on"/"1"/"true"/
/// "yes" enables, anything else falls back to the setting).
fn bell_enabled(setting: bool, env_value: Option<&str>) -> bool {
    match env_value.map(|value| value.trim().to_ascii_lowercase()) {
        Some(value) if matches!(value.as_str(), "off" | "0" | "false" | "no") => false,
        Some(value) if matches!(value.as_str(), "on" | "1" | "true" | "yes") => true,
        _ => setting,
    }
}

/// Bell gating: only ring for turns that ran long enough that the user has
/// plausibly tabbed away — rapid turns should never ding.
fn should_ring_bell(enabled: bool, working_for: Option<Duration>) -> bool {
    enabled && working_for.is_some_and(|elapsed| elapsed > BELL_MIN_WORKING_DURATION)
}

fn event_requests_immediate_draw(event: &Event) -> bool {
    matches!(
        event,
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp | MouseEventKind::ScrollDown,
            ..
        })
    )
}

/// A background `/workflow` run: its event channel plus the per-run
/// checkpoint recorder that captures pre-images for every file its subagents
/// mutate, so `/rewind` can undo a bad workflow. Cloning the recorder into the
/// worker's `ToolRuntime` shares state, so all subagent clones land in the
/// same checkpoint. The cancel token lets a turn cancel stop the run's tools.
struct BackgroundWorkflow {
    events: Receiver<WorkflowEvent>,
    checkpoint: CheckpointRecorder,
    cancel: CancelToken,
}

struct App {
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
    restart_requested: bool,
    cwd_display: String,
    inside_git_repo: bool,
    theme: ThemeKind,
    permission_mode: PermissionMode,
    tools: ToolRuntime,
    /// App-owned MCP registry, re-injected into every ToolRuntime rebuild so
    /// permission-mode switches never respawn live MCP servers.
    mcp: Arc<McpRegistry>,
    /// Snapshot rendered by the /mcp modal, captured when the command runs so
    /// drawing never blocks on a connecting server's state lock.
    mcp_statuses: Vec<McpServerStatus>,
    /// Snapshot rendered by the /agents modal, reloaded from .medusa/agents
    /// each time the command runs so file edits show up without a restart.
    agent_registry: AgentRegistry,
    model: DirectCodexBackend,
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

const COMPOSER_IMAGE_PREVIEW_WIDTH: u16 = 18;
const COMPOSER_IMAGE_PREVIEW_HEIGHT: u16 = 5;
const CHAT_IMAGE_PREVIEW_WIDTH: u16 = 52;
const CHAT_IMAGE_PREVIEW_HEIGHT: u16 = 16;
const IMAGE_PREVIEW_MIN_ZOOM: u16 = 25;
const IMAGE_PREVIEW_MAX_ZOOM: u16 = 300;
const IMAGE_PREVIEW_ZOOM_STEP: u16 = 25;
const CHAT_BOTTOM_PADDING_ROWS: usize = 1;
const MIN_TOOL_PULSE_VISIBLE: Duration = Duration::from_millis(650);
const DEFAULT_MODEL_CHOICES: &[&str] = &[
    "gpt-5.5",
    "gpt-5.3-codex",
    "gpt-5.3",
    "gpt-5.1-codex",
    "deepseek-v4-flash",
];
const SESSION_STATE_MAX_INTENTS: usize = 8;
const SESSION_STATE_MAX_OUTCOMES: usize = 8;
const SESSION_STATE_MAX_SYSTEM_NOTES: usize = 6;
const SESSION_STATE_MAX_TOOLS: usize = 12;
const SESSION_STATE_MAX_FILES: usize = 16;
const SESSION_MEMORY_MAX_PER_KIND: usize = 5;

const DOUBLE_ESCAPE_WINDOW: Duration = Duration::from_millis(1_500);
/// The @ mention file walk stops after this many files so giant workspaces
/// cannot stall the composer.
const MENTION_FILE_WALK_CAP: usize = 5_000;
/// At most this many fuzzy matches are kept for the mention popup.
const MENTION_MATCH_LIMIT: usize = 50;
/// Directories the @ mention file walk skips: VCS internals, caches, and
/// build output that would drown real sources.
const MENTION_SKIP_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    ".medusa",
    "target",
    "node_modules",
    "dist",
    "build",
    "out",
    ".next",
    ".cache",
    ".venv",
    "venv",
    "__pycache__",
    ".idea",
];
/// Turns shorter than this never ring the terminal bell.
const BELL_MIN_WORKING_DURATION: Duration = Duration::from_secs(10);
/// Heading written when quick-memory creates AGENTS.md from scratch.
const QUICK_MEMORY_HEADER: &str = "# Project notes";
/// Section of AGENTS.md that `# <note>` composer input appends to.
const QUICK_MEMORY_SECTION: &str = "## Notes";
/// Transcript note (and model-history system message) left when the user
/// interrupts a turn with Esc.
const TURN_INTERRUPTED_NOTE: &str = "turn interrupted by user";
/// Decision keys are ignored for this long after an approval prompt first
/// appears, so a keystroke already in flight can't blindly approve or deny.
const APPROVAL_KEY_GRACE: Duration = Duration::from_millis(350);

const PLAN_MODE_DIRECTIVE: &str = "Plan mode is active. Explore the workspace read-only to understand the task, \
then present a concise implementation plan: publish it with plan_update, raise decisions that materially change \
the approach with decision_request, and finish by asking the user to approve. Do not edit files, apply patches, \
or run mutating commands while plan mode is on; the user turns plan mode off to approve implementation.";

/// A tool call paused in a worker thread, waiting for the user's decision.
struct PendingApproval {
    request: ApprovalRequest,
    respond: Sender<ApprovalDecision>,
}

static ACTIVE_THEME: AtomicU8 = AtomicU8::new(0);

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ThemeKind {
    Medusa = 0,
    OpenCode = 1,
    TokyoNight = 2,
    Catppuccin = 3,
    Dracula = 4,
    Nord = 5,
    Gruvbox = 6,
    SolarizedDark = 7,
    MaterialDark = 8,
    MaterialTeal = 9,
    MaterialAmber = 10,
    MaterialIndigo = 11,
    MaterialRose = 12,
    RosePine = 13,
    AyuMirage = 14,
    Everforest = 15,
    Vesper = 16,
}

const THEME_KINDS: [ThemeKind; 17] = [
    ThemeKind::Medusa,
    ThemeKind::OpenCode,
    ThemeKind::TokyoNight,
    ThemeKind::Catppuccin,
    ThemeKind::Dracula,
    ThemeKind::Nord,
    ThemeKind::Gruvbox,
    ThemeKind::SolarizedDark,
    ThemeKind::MaterialDark,
    ThemeKind::MaterialTeal,
    ThemeKind::MaterialAmber,
    ThemeKind::MaterialIndigo,
    ThemeKind::MaterialRose,
    ThemeKind::RosePine,
    ThemeKind::AyuMirage,
    ThemeKind::Everforest,
    ThemeKind::Vesper,
];

#[derive(Debug, Clone, Copy)]
struct ThemePalette {
    text: Color,
    muted: Color,
    accent: Color,
    prompt: Color,
    separator: Color,
    selected_fg: Color,
    selected_bg: Color,
    activity_bg: Color,
    user_bg: Color,
    success: Color,
    error: Color,
    info: Color,
    tool: Color,
    quote: Color,
    code_fg: Color,
    code_bg: Color,
    inline_code_fg: Color,
    inline_code_bg: Color,
}

const MATERIAL_RED_400: Color = Color::Rgb(239, 83, 80);
const MATERIAL_PINK_300: Color = Color::Rgb(240, 98, 146);
const MATERIAL_PINK_200: Color = Color::Rgb(244, 143, 177);
const MATERIAL_DEEP_PURPLE_300: Color = Color::Rgb(149, 117, 205);
const MATERIAL_INDIGO_300: Color = Color::Rgb(121, 134, 203);
const MATERIAL_LIGHT_BLUE_300: Color = Color::Rgb(79, 195, 247);
const MATERIAL_CYAN_300: Color = Color::Rgb(77, 208, 225);
const MATERIAL_TEAL_200: Color = Color::Rgb(128, 203, 196);
const MATERIAL_TEAL_300: Color = Color::Rgb(77, 182, 172);
const MATERIAL_TEAL_400: Color = Color::Rgb(38, 166, 154);
const MATERIAL_GREEN_400: Color = Color::Rgb(102, 187, 106);
const MATERIAL_AMBER_300: Color = Color::Rgb(255, 213, 79);
const MATERIAL_AMBER_400: Color = Color::Rgb(255, 202, 40);
const MATERIAL_ORANGE_300: Color = Color::Rgb(255, 183, 77);
const MATERIAL_BLUE_GREY_50: Color = Color::Rgb(236, 239, 241);
const MATERIAL_BLUE_GREY_100: Color = Color::Rgb(207, 216, 220);
const MATERIAL_BLUE_GREY_200: Color = Color::Rgb(176, 190, 197);
const MATERIAL_BLUE_GREY_800: Color = Color::Rgb(55, 71, 79);
const MATERIAL_BLUE_GREY_900: Color = Color::Rgb(38, 50, 56);

fn material_dark_palette(
    accent: Color,
    prompt: Color,
    tool: Color,
    inline_code_fg: Color,
) -> ThemePalette {
    ThemePalette {
        text: MATERIAL_BLUE_GREY_50,
        muted: MATERIAL_BLUE_GREY_200,
        accent,
        prompt,
        separator: MATERIAL_BLUE_GREY_800,
        selected_fg: Color::Rgb(12, 18, 22),
        selected_bg: accent,
        activity_bg: MATERIAL_BLUE_GREY_900,
        user_bg: Color::Rgb(45, 35, 20),
        success: MATERIAL_GREEN_400,
        error: MATERIAL_RED_400,
        info: MATERIAL_CYAN_300,
        tool,
        quote: MATERIAL_BLUE_GREY_100,
        code_fg: MATERIAL_BLUE_GREY_50,
        code_bg: MATERIAL_BLUE_GREY_900,
        inline_code_fg,
        inline_code_bg: Color::Rgb(18, 31, 35),
    }
}

impl ThemeKind {
    fn from_workspace_settings(workspace: &Path) -> Self {
        Self::resolve(env::var("MEDUSA_THEME").ok().as_deref(), workspace)
    }

    /// Resolve the active theme from an explicit `MEDUSA_THEME`-style override
    /// (highest priority) then the persisted workspace settings. Taking the
    /// override as a parameter keeps tests off the process-global environment:
    /// `set_var` racing the parallel test harness's `getenv`-backed readers is
    /// undefined behavior.
    fn resolve(env_override: Option<&str>, workspace: &Path) -> Self {
        env_override
            .and_then(Self::from_name)
            .or_else(|| {
                load_app_settings(workspace)
                    .ok()
                    .and_then(|settings| settings.theme())
            })
            .unwrap_or(Self::Medusa)
    }

    fn from_name(name: &str) -> Option<Self> {
        let normalized = name.trim().to_ascii_lowercase().replace(['_', ' '], "-");

        match normalized.as_str() {
            "medusa" | "default" => Some(Self::Medusa),
            "opencode" | "open-code" => Some(Self::OpenCode),
            "tokyonight" | "tokyo-night" => Some(Self::TokyoNight),
            "catppuccin" | "mocha" => Some(Self::Catppuccin),
            "dracula" => Some(Self::Dracula),
            "nord" => Some(Self::Nord),
            "gruvbox" | "gruvbox-dark" => Some(Self::Gruvbox),
            "solarized" | "solarized-dark" => Some(Self::SolarizedDark),
            "material" | "material-dark" => Some(Self::MaterialDark),
            "material-teal" | "material-cyan" => Some(Self::MaterialTeal),
            "material-amber" | "material-yellow" => Some(Self::MaterialAmber),
            "material-indigo" | "material-purple" => Some(Self::MaterialIndigo),
            "material-rose" | "material-pink" => Some(Self::MaterialRose),
            "rose-pine" | "rosepine" => Some(Self::RosePine),
            "ayu" | "ayu-mirage" => Some(Self::AyuMirage),
            "everforest" | "everforest-dark" => Some(Self::Everforest),
            "vesper" => Some(Self::Vesper),
            _ => None,
        }
    }

    fn all() -> &'static [Self] {
        &THEME_KINDS
    }

    fn name(self) -> &'static str {
        match self {
            Self::Medusa => "medusa",
            Self::OpenCode => "opencode",
            Self::TokyoNight => "tokyonight",
            Self::Catppuccin => "catppuccin",
            Self::Dracula => "dracula",
            Self::Nord => "nord",
            Self::Gruvbox => "gruvbox",
            Self::SolarizedDark => "solarized-dark",
            Self::MaterialDark => "material-dark",
            Self::MaterialTeal => "material-teal",
            Self::MaterialAmber => "material-amber",
            Self::MaterialIndigo => "material-indigo",
            Self::MaterialRose => "material-rose",
            Self::RosePine => "rose-pine",
            Self::AyuMirage => "ayu-mirage",
            Self::Everforest => "everforest",
            Self::Vesper => "vesper",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Medusa => "Medusa",
            Self::OpenCode => "OpenCode",
            Self::TokyoNight => "Tokyo Night",
            Self::Catppuccin => "Catppuccin",
            Self::Dracula => "Dracula",
            Self::Nord => "Nord",
            Self::Gruvbox => "Gruvbox",
            Self::SolarizedDark => "Solarized Dark",
            Self::MaterialDark => "Material Dark",
            Self::MaterialTeal => "Material Teal",
            Self::MaterialAmber => "Material Amber",
            Self::MaterialIndigo => "Material Indigo",
            Self::MaterialRose => "Material Rose",
            Self::RosePine => "Rosé Pine",
            Self::AyuMirage => "Ayu Mirage",
            Self::Everforest => "Everforest",
            Self::Vesper => "Vesper",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::Medusa => "sharp black, acid green, warm prompt accents",
            Self::OpenCode => "quiet blue command surface with crisp contrast",
            Self::TokyoNight => "deep navy with cyan highlights",
            Self::Catppuccin => "soft mocha surface with rosewater accents",
            Self::Dracula => "inky violet with neon pink and green highlights",
            Self::Nord => "arctic blue-gray calm with frosty cyan accents",
            Self::Gruvbox => "retro warm earth tones with punchy orange prompts",
            Self::SolarizedDark => "low-glare teal base with balanced amber accents",
            Self::MaterialDark => "blue-grey Material base with balanced teal and amber",
            Self::MaterialTeal => "Material teal command surface with cyan tool accents",
            Self::MaterialAmber => "Material amber selection with teal prompts",
            Self::MaterialIndigo => "Material indigo focus with light-blue tooling",
            Self::MaterialRose => "Material rose accents with teal supporting signals",
            Self::RosePine => "muted rose and gold over a soho-night violet base",
            Self::AyuMirage => "dusky slate with warm orange and sky-blue accents",
            Self::Everforest => "soft forest greens with warm bark and sage tones",
            Self::Vesper => "near-black minimalism with a single peach accent",
        }
    }

    fn palette(self) -> ThemePalette {
        match self {
            Self::Medusa => ThemePalette {
                text: Color::Rgb(216, 216, 220),
                muted: Color::Rgb(132, 132, 142),
                accent: Color::Rgb(84, 214, 147),
                prompt: Color::Rgb(228, 169, 104),
                separator: Color::Rgb(35, 38, 42),
                selected_fg: Color::Rgb(10, 12, 14),
                selected_bg: Color::Rgb(84, 214, 147),
                activity_bg: Color::Rgb(18, 24, 30),
                user_bg: Color::Rgb(38, 30, 22),
                success: Color::Rgb(84, 214, 147),
                error: Color::Rgb(230, 111, 125),
                info: Color::Rgb(126, 176, 255),
                tool: Color::Rgb(126, 176, 255),
                quote: Color::Rgb(168, 176, 188),
                code_fg: Color::Rgb(190, 205, 220),
                code_bg: Color::Rgb(16, 18, 22),
                inline_code_fg: Color::Rgb(147, 210, 178),
                inline_code_bg: Color::Rgb(20, 26, 24),
            },
            Self::OpenCode => ThemePalette {
                text: Color::Rgb(221, 224, 229),
                muted: Color::Rgb(132, 139, 148),
                accent: Color::Rgb(96, 165, 250),
                prompt: Color::Rgb(245, 158, 11),
                separator: Color::Rgb(42, 46, 54),
                selected_fg: Color::Rgb(7, 10, 15),
                selected_bg: Color::Rgb(96, 165, 250),
                activity_bg: Color::Rgb(18, 27, 40),
                user_bg: Color::Rgb(42, 32, 18),
                success: Color::Rgb(52, 211, 153),
                error: Color::Rgb(248, 113, 113),
                info: Color::Rgb(147, 197, 253),
                tool: Color::Rgb(147, 197, 253),
                quote: Color::Rgb(176, 184, 196),
                code_fg: Color::Rgb(205, 213, 224),
                code_bg: Color::Rgb(17, 21, 28),
                inline_code_fg: Color::Rgb(191, 219, 254),
                inline_code_bg: Color::Rgb(23, 31, 44),
            },
            Self::TokyoNight => ThemePalette {
                text: Color::Rgb(192, 202, 245),
                muted: Color::Rgb(122, 162, 247),
                accent: Color::Rgb(125, 207, 255),
                prompt: Color::Rgb(255, 158, 100),
                separator: Color::Rgb(59, 66, 97),
                selected_fg: Color::Rgb(26, 27, 38),
                selected_bg: Color::Rgb(125, 207, 255),
                activity_bg: Color::Rgb(36, 40, 59),
                user_bg: Color::Rgb(49, 38, 36),
                success: Color::Rgb(158, 206, 106),
                error: Color::Rgb(247, 118, 142),
                info: Color::Rgb(125, 207, 255),
                tool: Color::Rgb(125, 207, 255),
                quote: Color::Rgb(154, 165, 206),
                code_fg: Color::Rgb(192, 202, 245),
                code_bg: Color::Rgb(22, 22, 30),
                inline_code_fg: Color::Rgb(187, 154, 247),
                inline_code_bg: Color::Rgb(38, 35, 58),
            },
            Self::Catppuccin => ThemePalette {
                text: Color::Rgb(205, 214, 244),
                muted: Color::Rgb(166, 173, 200),
                accent: Color::Rgb(137, 220, 235),
                prompt: Color::Rgb(250, 179, 135),
                separator: Color::Rgb(69, 71, 90),
                selected_fg: Color::Rgb(17, 17, 27),
                selected_bg: Color::Rgb(137, 220, 235),
                activity_bg: Color::Rgb(30, 30, 46),
                user_bg: Color::Rgb(51, 39, 39),
                success: Color::Rgb(166, 227, 161),
                error: Color::Rgb(243, 139, 168),
                info: Color::Rgb(137, 180, 250),
                tool: Color::Rgb(137, 180, 250),
                quote: Color::Rgb(180, 190, 254),
                code_fg: Color::Rgb(203, 214, 244),
                code_bg: Color::Rgb(24, 24, 37),
                inline_code_fg: Color::Rgb(148, 226, 213),
                inline_code_bg: Color::Rgb(30, 30, 46),
            },
            Self::Dracula => ThemePalette {
                text: Color::Rgb(248, 248, 242),
                muted: Color::Rgb(139, 143, 173),
                accent: Color::Rgb(189, 147, 249),
                prompt: Color::Rgb(255, 184, 108),
                separator: Color::Rgb(68, 71, 90),
                selected_fg: Color::Rgb(40, 42, 54),
                selected_bg: Color::Rgb(189, 147, 249),
                activity_bg: Color::Rgb(40, 42, 54),
                user_bg: Color::Rgb(50, 43, 38),
                success: Color::Rgb(80, 250, 123),
                error: Color::Rgb(255, 85, 85),
                info: Color::Rgb(139, 233, 253),
                tool: Color::Rgb(139, 233, 253),
                quote: Color::Rgb(241, 250, 140),
                code_fg: Color::Rgb(248, 248, 242),
                code_bg: Color::Rgb(33, 34, 44),
                inline_code_fg: Color::Rgb(255, 121, 198),
                inline_code_bg: Color::Rgb(48, 42, 65),
            },
            Self::Nord => ThemePalette {
                text: Color::Rgb(216, 222, 233),
                muted: Color::Rgb(129, 161, 193),
                accent: Color::Rgb(136, 192, 208),
                prompt: Color::Rgb(235, 203, 139),
                separator: Color::Rgb(67, 76, 94),
                selected_fg: Color::Rgb(46, 52, 64),
                selected_bg: Color::Rgb(136, 192, 208),
                activity_bg: Color::Rgb(59, 66, 82),
                user_bg: Color::Rgb(70, 61, 48),
                success: Color::Rgb(163, 190, 140),
                error: Color::Rgb(191, 97, 106),
                info: Color::Rgb(129, 161, 193),
                tool: Color::Rgb(129, 161, 193),
                quote: Color::Rgb(180, 142, 173),
                code_fg: Color::Rgb(229, 233, 240),
                code_bg: Color::Rgb(36, 42, 54),
                inline_code_fg: Color::Rgb(143, 188, 187),
                inline_code_bg: Color::Rgb(48, 56, 70),
            },
            Self::Gruvbox => ThemePalette {
                text: Color::Rgb(235, 219, 178),
                muted: Color::Rgb(168, 153, 132),
                accent: Color::Rgb(184, 187, 38),
                prompt: Color::Rgb(254, 128, 25),
                separator: Color::Rgb(80, 73, 69),
                selected_fg: Color::Rgb(40, 40, 40),
                selected_bg: Color::Rgb(250, 189, 47),
                activity_bg: Color::Rgb(60, 56, 54),
                user_bg: Color::Rgb(66, 49, 35),
                success: Color::Rgb(184, 187, 38),
                error: Color::Rgb(251, 73, 52),
                info: Color::Rgb(131, 165, 152),
                tool: Color::Rgb(131, 165, 152),
                quote: Color::Rgb(211, 134, 155),
                code_fg: Color::Rgb(235, 219, 178),
                code_bg: Color::Rgb(29, 32, 33),
                inline_code_fg: Color::Rgb(250, 189, 47),
                inline_code_bg: Color::Rgb(50, 48, 47),
            },
            Self::SolarizedDark => ThemePalette {
                text: Color::Rgb(131, 148, 150),
                muted: Color::Rgb(88, 110, 117),
                accent: Color::Rgb(42, 161, 152),
                prompt: Color::Rgb(181, 137, 0),
                separator: Color::Rgb(7, 54, 66),
                selected_fg: Color::Rgb(0, 43, 54),
                selected_bg: Color::Rgb(42, 161, 152),
                activity_bg: Color::Rgb(7, 54, 66),
                user_bg: Color::Rgb(58, 49, 15),
                success: Color::Rgb(133, 153, 0),
                error: Color::Rgb(220, 50, 47),
                info: Color::Rgb(38, 139, 210),
                tool: Color::Rgb(38, 139, 210),
                quote: Color::Rgb(108, 113, 196),
                code_fg: Color::Rgb(147, 161, 161),
                code_bg: Color::Rgb(0, 35, 44),
                inline_code_fg: Color::Rgb(203, 75, 22),
                inline_code_bg: Color::Rgb(7, 54, 66),
            },
            Self::MaterialDark => material_dark_palette(
                MATERIAL_TEAL_300,
                MATERIAL_AMBER_400,
                MATERIAL_CYAN_300,
                MATERIAL_TEAL_200,
            ),
            Self::MaterialTeal => material_dark_palette(
                MATERIAL_TEAL_400,
                MATERIAL_ORANGE_300,
                MATERIAL_CYAN_300,
                MATERIAL_TEAL_200,
            ),
            Self::MaterialAmber => material_dark_palette(
                MATERIAL_AMBER_400,
                MATERIAL_TEAL_300,
                MATERIAL_ORANGE_300,
                MATERIAL_AMBER_300,
            ),
            Self::MaterialIndigo => material_dark_palette(
                MATERIAL_INDIGO_300,
                MATERIAL_AMBER_300,
                MATERIAL_LIGHT_BLUE_300,
                MATERIAL_DEEP_PURPLE_300,
            ),
            Self::MaterialRose => material_dark_palette(
                MATERIAL_PINK_300,
                MATERIAL_AMBER_300,
                MATERIAL_TEAL_200,
                MATERIAL_PINK_200,
            ),
            Self::RosePine => ThemePalette {
                text: Color::Rgb(224, 222, 244),
                muted: Color::Rgb(144, 140, 170),
                accent: Color::Rgb(235, 188, 186),
                prompt: Color::Rgb(246, 193, 119),
                separator: Color::Rgb(38, 35, 58),
                selected_fg: Color::Rgb(25, 23, 36),
                selected_bg: Color::Rgb(235, 188, 186),
                activity_bg: Color::Rgb(31, 29, 46),
                user_bg: Color::Rgb(42, 33, 24),
                success: Color::Rgb(156, 207, 216),
                error: Color::Rgb(235, 111, 146),
                info: Color::Rgb(196, 167, 231),
                tool: Color::Rgb(156, 207, 216),
                quote: Color::Rgb(184, 179, 209),
                code_fg: Color::Rgb(224, 222, 244),
                code_bg: Color::Rgb(31, 29, 46),
                inline_code_fg: Color::Rgb(196, 167, 231),
                inline_code_bg: Color::Rgb(38, 35, 58),
            },
            Self::AyuMirage => ThemePalette {
                text: Color::Rgb(203, 204, 198),
                muted: Color::Rgb(112, 122, 140),
                accent: Color::Rgb(115, 208, 255),
                prompt: Color::Rgb(255, 167, 89),
                separator: Color::Rgb(51, 65, 94),
                selected_fg: Color::Rgb(31, 36, 48),
                selected_bg: Color::Rgb(115, 208, 255),
                activity_bg: Color::Rgb(35, 40, 52),
                user_bg: Color::Rgb(48, 38, 24),
                success: Color::Rgb(186, 230, 126),
                error: Color::Rgb(255, 102, 102),
                info: Color::Rgb(92, 207, 230),
                tool: Color::Rgb(92, 207, 230),
                quote: Color::Rgb(166, 172, 205),
                code_fg: Color::Rgb(203, 204, 198),
                code_bg: Color::Rgb(36, 41, 54),
                inline_code_fg: Color::Rgb(149, 230, 203),
                inline_code_bg: Color::Rgb(42, 48, 62),
            },
            Self::Everforest => ThemePalette {
                text: Color::Rgb(211, 198, 170),
                muted: Color::Rgb(133, 146, 137),
                accent: Color::Rgb(167, 192, 128),
                prompt: Color::Rgb(230, 152, 117),
                separator: Color::Rgb(71, 82, 88),
                selected_fg: Color::Rgb(45, 53, 59),
                selected_bg: Color::Rgb(167, 192, 128),
                activity_bg: Color::Rgb(52, 63, 68),
                user_bg: Color::Rgb(58, 49, 37),
                success: Color::Rgb(167, 192, 128),
                error: Color::Rgb(230, 126, 128),
                info: Color::Rgb(127, 187, 179),
                tool: Color::Rgb(127, 187, 179),
                quote: Color::Rgb(157, 169, 160),
                code_fg: Color::Rgb(211, 198, 170),
                code_bg: Color::Rgb(39, 46, 51),
                inline_code_fg: Color::Rgb(131, 192, 146),
                inline_code_bg: Color::Rgb(47, 56, 62),
            },
            Self::Vesper => ThemePalette {
                text: Color::Rgb(209, 209, 209),
                muted: Color::Rgb(118, 118, 118),
                accent: Color::Rgb(255, 199, 153),
                prompt: Color::Rgb(255, 199, 153),
                separator: Color::Rgb(40, 40, 40),
                selected_fg: Color::Rgb(16, 16, 16),
                selected_bg: Color::Rgb(255, 199, 153),
                activity_bg: Color::Rgb(24, 24, 24),
                user_bg: Color::Rgb(38, 30, 22),
                success: Color::Rgb(153, 255, 228),
                error: Color::Rgb(255, 128, 128),
                info: Color::Rgb(153, 255, 228),
                tool: Color::Rgb(172, 172, 172),
                quote: Color::Rgb(160, 160, 160),
                code_fg: Color::Rgb(209, 209, 209),
                code_bg: Color::Rgb(20, 20, 20),
                inline_code_fg: Color::Rgb(255, 199, 153),
                inline_code_bg: Color::Rgb(30, 30, 30),
            },
        }
    }
}

fn set_active_theme(theme: ThemeKind) {
    ACTIVE_THEME.store(theme as u8, Ordering::Relaxed);
}

fn theme_index(theme: ThemeKind) -> usize {
    ThemeKind::all()
        .iter()
        .position(|candidate| *candidate == theme)
        .unwrap_or(0)
}

fn theme_at_offset(theme: ThemeKind, offset: isize) -> ThemeKind {
    let themes = ThemeKind::all();
    let next = (theme_index(theme) as isize + offset).rem_euclid(themes.len() as isize) as usize;
    themes[next]
}

/// Selectable model slugs. Primary source is Codex's own backend model cache
/// (`~/.codex/models_cache.json`), so the picker reflects exactly what the
/// account can use — new models appear with no code change. Falls back to a
/// built-in list when the cache is absent (non-Codex provider / fresh install).
/// The current model is always present, pinned first if the source omits it.
fn model_choices(current: &str) -> Vec<String> {
    let mut choices = medusa_core::models::codex_backend_models()
        .map(|models| {
            models
                .into_iter()
                .map(|model| model.slug)
                .collect::<Vec<_>>()
        })
        .filter(|slugs: &Vec<String>| !slugs.is_empty())
        .unwrap_or_else(|| {
            DEFAULT_MODEL_CHOICES
                .iter()
                .map(|model| (*model).to_string())
                .collect()
        });
    let current = current.trim();
    if !current.is_empty() && !choices.iter().any(|model| model == current) {
        choices.insert(0, current.to_string());
    }
    choices
}

/// Display label + optional description for a model slug, from the Codex
/// backend cache. Unknown slugs (custom/env-set models) render as the slug.
fn model_display(slug: &str) -> (String, Option<String>) {
    medusa_core::models::codex_backend_models()
        .and_then(|models| models.into_iter().find(|model| model.slug == slug))
        .map(|model| (model.display_name, model.description))
        .unwrap_or_else(|| (slug.to_string(), None))
}

fn model_index(current: &str) -> usize {
    model_choices(current)
        .iter()
        .position(|model| model == current)
        .unwrap_or(0)
}

/// Reasoning efforts selectable for `model`: the backend's per-model list when
/// known, else standard defaults. The active effort is always present.
fn reasoning_choices(model: &str, current: &str) -> Vec<String> {
    let mut choices = medusa_core::models::reasoning_efforts_for(model)
        .into_iter()
        .map(|level| level.effort)
        .collect::<Vec<_>>();
    let current = current.trim();
    if !current.is_empty() && !choices.iter().any(|effort| effort == current) {
        choices.push(current.to_string());
    }
    choices
}

fn reasoning_index(model: &str, current: &str) -> usize {
    reasoning_choices(model, current)
        .iter()
        .position(|effort| effort == current)
        .unwrap_or(0)
}

/// Backend description for a reasoning effort of a model, when the cache has one.
fn reasoning_description(model: &str, effort: &str) -> Option<String> {
    medusa_core::models::reasoning_efforts_for(model)
        .into_iter()
        .find(|level| level.effort == effort)
        .and_then(|level| level.description)
}

fn permission_mode_index(mode: PermissionMode) -> usize {
    PermissionMode::all()
        .iter()
        .position(|candidate| *candidate == mode)
        .unwrap_or(0)
}

fn active_theme() -> ThemeKind {
    match ACTIVE_THEME.load(Ordering::Relaxed) {
        1 => ThemeKind::OpenCode,
        2 => ThemeKind::TokyoNight,
        3 => ThemeKind::Catppuccin,
        4 => ThemeKind::Dracula,
        5 => ThemeKind::Nord,
        6 => ThemeKind::Gruvbox,
        7 => ThemeKind::SolarizedDark,
        8 => ThemeKind::MaterialDark,
        9 => ThemeKind::MaterialTeal,
        10 => ThemeKind::MaterialAmber,
        11 => ThemeKind::MaterialIndigo,
        12 => ThemeKind::MaterialRose,
        13 => ThemeKind::RosePine,
        14 => ThemeKind::AyuMirage,
        15 => ThemeKind::Everforest,
        16 => ThemeKind::Vesper,
        _ => ThemeKind::Medusa,
    }
}

fn palette() -> ThemePalette {
    active_theme().palette()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ChatViewportMetrics {
    text_area: Rect,
    has_scrollbar: bool,
    total_visual_lines: usize,
    max_scroll: usize,
    scroll: usize,
    top_offset: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Modal {
    Commands,
    Settings,
    Help,
    ImagePreview,
    Workflows,
    Jobs,
    Sessions,
    SessionTree,
    Models,
    Reasoning,
    Permissions,
    Themes,
    Rewind,
    EditMessage,
    Mcp,
    Agents,
    Cost,
    Context,
}

/// Which screen of the /rewind modal is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RewindStage {
    Pick,
    Confirm,
}

/// One row in the /edit backtrack picker: a previous user message.
#[derive(Debug, Clone)]
struct EditPickerEntry {
    transcript_index: usize,
    preview: String,
}

/// The /edit picker shows at most this many previous user messages.
const EDIT_PICKER_LIMIT: usize = 20;

#[derive(Debug, Clone)]
struct Toast {
    message: String,
    kind: ToastKind,
    created_at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToastKind {
    Info,
    Success,
    Warning,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UiFocus {
    Composer,
    Activity,
    Modal,
    Transcript,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ChatMessage {
    role: ChatRole,
    content: String,
    #[serde(default)]
    attachments: Vec<ImageAttachment>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ImageAttachment {
    id: String,
    name: String,
    path: PathBuf,
    mime: String,
    width: u32,
    height: u32,
    size_bytes: u64,
}

#[derive(Debug, Clone)]
struct TranscriptRow {
    line: Line<'static>,
    image: Option<ImageAttachment>,
}

impl TranscriptRow {
    fn text(line: Line<'static>) -> Self {
        Self { line, image: None }
    }

    fn image(line: Line<'static>, attachment: ImageAttachment) -> Self {
        Self {
            line,
            image: Some(attachment),
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct RenderContext {
    animation_tick: u64,
    /// Index of the keyboard-selected question in the pending decision.
    decision_selection: usize,
}

impl RenderContext {
    #[cfg(test)]
    fn static_view() -> Self {
        Self::default()
    }
}

struct TerminalImageRenderer {
    picker: Option<Picker>,
    protocols: HashMap<String, SlicedProtocol>,
}

impl std::fmt::Debug for TerminalImageRenderer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TerminalImageRenderer")
            .field("enabled", &self.is_enabled())
            .field("cached", &self.protocols.len())
            .finish()
    }
}

impl TerminalImageRenderer {
    fn detect() -> Self {
        if env::var("MEDUSA_DISABLE_IMAGES").is_ok_and(|value| value == "1" || value == "true") {
            return Self::disabled();
        }

        #[cfg(test)]
        {
            Self::disabled()
        }

        #[cfg(not(test))]
        {
            let picker = Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks());
            Self {
                picker: Some(picker),
                protocols: HashMap::new(),
            }
        }
    }

    fn disabled() -> Self {
        Self {
            picker: None,
            protocols: HashMap::new(),
        }
    }

    fn is_enabled(&self) -> bool {
        self.picker.is_some()
    }

    #[allow(clippy::too_many_arguments)]
    fn render(
        &mut self,
        frame: &mut Frame<'_>,
        attachment: &ImageAttachment,
        area: Rect,
        width: u16,
        height: u16,
        x_offset: u16,
        y_offset: i16,
    ) -> bool {
        if area.width == 0 || area.height == 0 || width == 0 || height == 0 {
            return false;
        }

        let Some(protocol) = self.protocol_for(attachment, width, height) else {
            return false;
        };

        frame.render_widget(
            SlicedImage::new(
                protocol,
                SignedPosition::from((x_offset.min(i16::MAX as u16) as i16, y_offset)),
            ),
            area,
        );
        true
    }

    fn protocol_for(
        &mut self,
        attachment: &ImageAttachment,
        width: u16,
        height: u16,
    ) -> Option<&SlicedProtocol> {
        let picker = self.picker.as_ref()?;
        let key = format!("{}:{width}x{height}", attachment.id);
        if !self.protocols.contains_key(&key) {
            let bytes = fs::read(&attachment.path).ok()?;
            let image = image::load_from_memory(&bytes).ok()?;
            let protocol = SlicedProtocol::new_with_resize(
                picker,
                image,
                (width, height).into(),
                Resize::Fit(None),
            )
            .ok()?;
            self.protocols.insert(key.clone(), protocol);
        }
        self.protocols.get(&key)
    }

    fn forget(&mut self, attachment_id: &str) {
        let prefix = format!("{attachment_id}:");
        self.protocols.retain(|key, _| !key.starts_with(&prefix));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum ChatRole {
    User,
    Assistant,
    Tool,
    System,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum TranscriptItem {
    Message(ChatMessage),
    Tool(ToolRun),
    Reasoning(ReasoningTrace),
    Plan(PlanView),
    Decision(DecisionView),
    Workflow(WorkflowRunView),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ToolRun {
    #[serde(default)]
    id: Option<String>,
    #[serde(skip, default = "Instant::now")]
    started_at: Instant,
    #[serde(skip, default)]
    pending_result: Option<ToolRunPendingResult>,
    name: String,
    summary: String,
    state: ToolRunState,
    detail: String,
    #[serde(default)]
    expanded: bool,
    /// Set on the first tool of a finished group to re-open a collapsed group.
    #[serde(default)]
    group_expanded: bool,
}

#[derive(Debug, Clone)]
struct TranscriptRowsCache {
    version: u64,
    theme: ThemeKind,
    streaming_message: Option<usize>,
    selected_tool: Option<usize>,
    animation_tick: Option<u64>,
    decision_selection: usize,
    /// Shared so cache hits are an Arc bump, not a deep clone of every row.
    rows: Arc<Vec<TranscriptRow>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ReasoningTrace {
    content: String,
    #[serde(default)]
    expanded: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PlanView {
    #[serde(default)]
    summary: String,
    items: Vec<PlanItemView>,
    #[serde(default)]
    expanded: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PlanItemView {
    text: String,
    status: PlanItemStatus,
    #[serde(default)]
    evidence: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PlanItemStatus {
    Pending,
    Active,
    Done,
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct DecisionView {
    #[serde(default)]
    title: String,
    #[serde(default)]
    reason: String,
    questions: Vec<DecisionQuestionView>,
    #[serde(default)]
    assumptions: Vec<String>,
    #[serde(default)]
    answered: bool,
    #[serde(default)]
    answer: Option<String>,
    #[serde(default)]
    answers: BTreeMap<String, String>,
    #[serde(default)]
    expanded: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct DecisionQuestionView {
    id: String,
    prompt: String,
    kind: DecisionQuestionKind,
    #[serde(default)]
    options: Vec<String>,
    #[serde(default)]
    recommended: Option<String>,
    #[serde(default = "default_required_decision")]
    required: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum DecisionQuestionKind {
    Choice,
    Text,
}

fn default_required_decision() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct WorkflowRunView {
    id: String,
    title: String,
    task: String,
    status: WorkflowViewState,
    phases: Vec<WorkflowPhaseView>,
    summary: String,
    #[serde(default)]
    expanded: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct WorkflowPhaseView {
    name: String,
    objective: String,
    status: WorkflowViewState,
    agents: Vec<WorkflowAgentView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct WorkflowAgentView {
    name: String,
    role: String,
    #[serde(default)]
    tool_policy: SubagentToolPolicy,
    status: WorkflowViewState,
    output: String,
    #[serde(default)]
    tool_counts: BTreeMap<String, usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum WorkflowViewState {
    Pending,
    Running,
    Succeeded,
    PartiallySucceeded,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum ToolRunState {
    Running,
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ToolRunPendingResult {
    state: ToolRunState,
    detail: String,
    received_at: Instant,
}

fn queue_or_apply_tool_result(
    run: &mut ToolRun,
    state: ToolRunState,
    detail: String,
    expand_on_failure: bool,
) {
    if run.started_at.elapsed() < MIN_TOOL_PULSE_VISIBLE {
        run.pending_result = Some(ToolRunPendingResult {
            state,
            detail,
            received_at: Instant::now(),
        });
        return;
    }
    apply_tool_result_now(run, state, detail, expand_on_failure);
}

fn apply_tool_result_now(
    run: &mut ToolRun,
    state: ToolRunState,
    detail: String,
    expand_on_failure: bool,
) {
    run.state = state;
    run.detail = detail;
    run.pending_result = None;
    run.expanded = expand_on_failure && state == ToolRunState::Failed;
}

/// Sweep an interrupted workflow row: everything still running (the run, its
/// phases, their agents) resolves to Failed so no spinner survives the turn.
fn mark_workflow_view_cancelled(view: &mut WorkflowRunView) {
    if view.status != WorkflowViewState::Running {
        return;
    }
    view.status = WorkflowViewState::Failed;
    if view.summary.is_empty() {
        view.summary = "cancelled".to_string();
    }
    for phase in &mut view.phases {
        if phase.status == WorkflowViewState::Running {
            phase.status = WorkflowViewState::Failed;
        }
        for agent in &mut phase.agents {
            if agent.status == WorkflowViewState::Running {
                agent.status = WorkflowViewState::Failed;
            }
        }
    }
}

impl ChatMessage {
    fn user(content: impl Into<String>) -> Self {
        Self {
            role: ChatRole::User,
            content: content.into(),
            attachments: Vec::new(),
        }
    }

    fn user_with_attachments(
        content: impl Into<String>,
        attachments: Vec<ImageAttachment>,
    ) -> Self {
        Self {
            role: ChatRole::User,
            content: content.into(),
            attachments,
        }
    }

    fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: ChatRole::Assistant,
            content: content.into(),
            attachments: Vec::new(),
        }
    }

    fn system(content: impl Into<String>) -> Self {
        Self {
            role: ChatRole::System,
            content: content.into(),
            attachments: Vec::new(),
        }
    }
}

impl App {
    fn new(startup_session: SessionOpenMode) -> Result<Self> {
        Self::with_model_backend_and_session(true, startup_session)
    }

    #[cfg(test)]
    fn with_model_backend(model_enabled: bool) -> Self {
        // Each test app gets its own workspace: parallel tests sharing the
        // real cwd raced on .medusa/permissions.json and flaked.
        use std::sync::atomic::AtomicU64;
        static NEXT_TEST_WORKSPACE: AtomicU64 = AtomicU64::new(0);
        let dir = env::temp_dir().join(format!(
            "medusa-test-{}-{}",
            std::process::id(),
            NEXT_TEST_WORKSPACE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).expect("test workspace should be creatable");
        Self::build_in(model_enabled, None, Some(dir))
    }

    fn with_model_backend_and_session(
        model_enabled: bool,
        startup_session: SessionOpenMode,
    ) -> Result<Self> {
        let cwd = env::current_dir().unwrap_or_else(|_| Path::new(".").to_path_buf());
        let session = SessionStore::open(&cwd, startup_session)?;
        let transcript = session.load_transcript_with_legacy(TranscriptItem::Message)?;
        let mut app = Self::build(model_enabled, Some(session));
        if !transcript.is_empty() {
            app.transcript = transcript;
            app.touch_transcript();
            app.status_line = "session restored".to_string();
        } else {
            app.persist_session();
        }
        Ok(app)
    }

    fn build(model_enabled: bool, session: Option<SessionStore>) -> Self {
        Self::build_in(model_enabled, session, None)
    }

    fn build_in(
        model_enabled: bool,
        session: Option<SessionStore>,
        workspace: Option<PathBuf>,
    ) -> Self {
        let cwd = workspace
            .or_else(|| env::current_dir().ok())
            .unwrap_or_else(|| Path::new(".").to_path_buf());
        let tools = ToolRuntime::new(&cwd).expect("current directory should be usable");
        let (mcp, mcp_load_error) = match McpRegistry::load(tools.workspace()) {
            Ok(registry) => (registry, None),
            Err(error) => (McpRegistry::empty(), Some(error.to_string())),
        };
        let tools = tools.with_mcp(mcp.clone());
        // Deliberately do NOT prewarm MCP servers here: spawning a server runs
        // an arbitrary command from `.medusa/mcp.json`, and a freshly-cloned
        // untrusted repo must not execute those without a human click. Servers
        // start lazily on first use, gated by a launch approval in every
        // confined mode (Open mode trusts the workspace config). See
        // ToolRuntime::mcp_tool_schemas / authorize_mcp_launch.
        let app_settings = load_app_settings(tools.workspace()).unwrap_or_default();
        let mut model =
            DirectCodexBackend::new(tools.workspace().to_path_buf()).expect("HTTP client builds");
        if env::var_os("MEDUSA_MODEL").is_none()
            && let Some(model_name) = app_settings.model()
        {
            model.set_model_name(model_name);
        }
        // Env override wins for one-off launches; else the saved preference.
        if env::var_os("MEDUSA_REASONING_EFFORT").is_none()
            && let Some(effort) = app_settings.reasoning_effort()
        {
            model.set_reasoning_effort(effort);
        }
        let cwd_display = abbreviate_home(&tools.workspace().to_string_lossy());
        let inside_git_repo = Path::new(".git").exists();
        let theme = ThemeKind::from_workspace_settings(tools.workspace());
        let permission_mode = app_settings.permission_mode();
        set_active_theme(theme);
        let (background_job_sender, background_job_events) = mpsc::channel();
        let (approval_sender, approval_events) = mpsc::channel::<PendingApproval>();
        let approval_handler: medusa_core::tools::ApprovalHandler =
            Arc::new(move |request: ApprovalRequest| {
                let (respond, decision) = mpsc::channel();
                if approval_sender
                    .send(PendingApproval { request, respond })
                    .is_err()
                {
                    return ApprovalDecision::Deny;
                }
                decision.recv().unwrap_or(ApprovalDecision::Deny)
            });

        let mut app = Self {
            input: String::new(),
            input_cursor: 0,
            pending_attachments: Vec::new(),
            attachment_previews: HashMap::new(),
            image_renderer: TerminalImageRenderer::detect(),
            transcript: Vec::new(),
            transcript_version: 0,
            transcript_rows_cache: None,
            status_line: "Ready.".to_string(),
            last_chat_viewport: None,
            last_transcript_rows: Arc::new(Vec::new()),
            should_quit: false,
            restart_requested: false,
            cwd_display,
            inside_git_repo,
            theme,
            permission_mode,
            tools,
            mcp,
            mcp_statuses: Vec::new(),
            agent_registry: AgentRegistry::default(),
            context_engine: ContextEngine::new(),
            plan_mode: false,
            last_escape_at: None,
            model,
            model_enabled,
            model_events: None,
            workflow_events: Vec::new(),
            background_job_sender,
            approval_handler,
            approval_events,
            approval_queue: VecDeque::new(),
            session_terminal_grants: Vec::new(),
            session_edit_grants: Vec::new(),
            session_web_egress_allowed: false,
            denied_this_turn: Vec::new(),
            approval_shown_at: None,
            denied_edits_this_turn: Vec::new(),
            background_job_events,
            background_jobs: BTreeMap::new(),
            streaming_message: None,
            queued_turns: VecDeque::new(),
            turn_cancel: None,
            cancel_requested_at: None,
            last_stream_save: Instant::now(),
            chat_scroll: 0,
            chat_scroll_target: 0,
            selected_tool: None,
            decision_selection: 0,
            workflows: Vec::new(),
            animation_tick: 0,
            started_at: Instant::now(),
            turn_started_at: None,
            session,
            active_modal: None,
            slash_selection: 0,
            mention_selection: 0,
            mention_files: None,
            mention_dismissed: false,
            bell_setting: app_settings.bell.unwrap_or(true),
            settings_selection: 0,
            model_selection: 0,
            reasoning_selection: 0,
            permission_selection: permission_mode_index(permission_mode),
            theme_selection: theme_index(theme),
            image_preview_index: 0,
            image_preview_zoom: 100,
            theme_preview_original: None,
            toast: None,
            session_usage: TokenUsage::default(),
            session_requests: 0,
            turn_usage: TokenUsage::default(),
            turn_requests: 0,
            last_turn_usage: TokenUsage::default(),
            last_turn_requests: 0,
            context_report: None,
            compact_events: None,
            active_checkpoint: None,
            #[cfg(test)]
            last_turn_runtime: None,
            #[cfg(test)]
            last_workflow_runtime: None,
            rewind_entries: Vec::new(),
            rewind_selection: 0,
            rewind_stage: RewindStage::Pick,
            rewind_confirm_selection: 0,
            edit_picker_entries: Vec::new(),
            edit_picker_selection: 0,
            review_diff_check: workspace_has_reviewable_diff,
        };
        if let Some(error) = mcp_load_error {
            app.toast(format!("MCP config ignored: {error}"), ToastKind::Warning);
        }
        app
    }

    fn run(&mut self, terminal: &mut Tui) -> Result<()> {
        let mut needs_draw = true;
        let mut terminal_changed = true;
        let mut last_draw = Instant::now() - Duration::from_secs(1);

        while !self.should_quit {
            terminal_changed |= self.drain_terminal_events(Duration::ZERO)?;
            if self.should_quit {
                break;
            }

            let previous_animation_tick = self.animation_tick;
            self.animation_tick = self.animation_frame();
            let animated = self.has_active_animation();
            let animation_changed = animated && self.animation_tick != previous_animation_tick;
            let toast_changed = self.expire_toast();
            let model_changed = self.drain_model_events();
            let workflow_changed = self.drain_workflow_events();
            let background_changed = self.drain_background_job_events();
            let approval_changed = self.drain_approval_requests();
            let pending_tool_changed = self.drain_pending_tool_results();
            let compact_changed = self.drain_compact_events();

            needs_draw |= terminal_changed
                || toast_changed
                || model_changed
                || workflow_changed
                || background_changed
                || approval_changed
                || pending_tool_changed
                || compact_changed
                || animation_changed;

            let frame_cadence = if animated {
                Duration::from_millis(16)
            } else {
                Duration::from_millis(50)
            };

            if needs_draw && (terminal_changed || last_draw.elapsed() >= frame_cadence) {
                terminal::draw_synchronized(terminal, |terminal| {
                    terminal.draw(|frame| self.draw(frame)).map(|_| ())
                })?;
                last_draw = Instant::now();
                needs_draw = false;
                terminal_changed = false;
            }

            self.clamp_chat_scroll_to_viewport();
            let poll_interval = if needs_draw {
                frame_cadence
                    .saturating_sub(last_draw.elapsed())
                    .min(Duration::from_millis(16))
            } else if animated {
                Duration::from_millis(16)
            } else if self.toast.is_some() {
                Duration::from_millis(100)
            } else {
                Duration::from_millis(250)
            };

            terminal_changed |= self.drain_terminal_events(poll_interval)?;
        }

        Ok(())
    }

    fn drain_terminal_events(&mut self, initial_timeout: Duration) -> Result<bool> {
        if !event::poll(initial_timeout)? {
            return Ok(false);
        }

        let event = event::read()?;
        let should_draw = event_requests_immediate_draw(&event);
        self.handle_terminal_event(event);
        if should_draw {
            return Ok(true);
        }

        for _ in 0..128 {
            if self.should_quit || !event::poll(Duration::ZERO)? {
                break;
            }
            let event = event::read()?;
            let should_draw = event_requests_immediate_draw(&event);
            self.handle_terminal_event(event);
            if should_draw {
                break;
            }
        }

        Ok(true)
    }

    fn handle_terminal_event(&mut self, event: Event) {
        match event {
            Event::Key(key) => self.handle_key(key),
            Event::Paste(text) => self.handle_paste(text),
            Event::Mouse(mouse) => self.handle_mouse(mouse),
            _ => {}
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }

        // Approval prompts take priority over every other surface: a worker
        // thread is blocked waiting on this answer.
        if !self.approval_queue.is_empty() {
            // Ignore (but consume) keystrokes for a brief window after the
            // prompt appears so an in-flight keypress can't blindly decide.
            if self
                .approval_shown_at
                .is_none_or(|shown| shown.elapsed() < APPROVAL_KEY_GRACE)
            {
                if self.approval_shown_at.is_none() {
                    self.approval_shown_at = Some(Instant::now());
                }
                return;
            }
            if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                self.should_quit = true;
                return;
            }
            // Decision keys must be unmodified: Ctrl+A (readline home) and the
            // like must never approve or persist a grant.
            let plain = key.modifiers.difference(KeyModifiers::SHIFT).is_empty();
            if plain {
                match key.code {
                    KeyCode::Char('y') | KeyCode::Char('Y') => {
                        self.resolve_pending_approval(ApprovalDecision::AllowOnce);
                    }
                    KeyCode::Char('a') | KeyCode::Char('A') => {
                        // Escalation cards do not offer always-allow: a
                        // persisted grant must never silently unsandbox
                        // future runs.
                        if self
                            .approval_queue
                            .front()
                            .is_none_or(|pending| !pending.request.sandbox_escalation)
                        {
                            self.resolve_pending_approval(ApprovalDecision::AlwaysAllow);
                        }
                    }
                    KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                        self.last_escape_at = None;
                        self.resolve_pending_approval(ApprovalDecision::Deny);
                    }
                    _ => {}
                }
            }
            return;
        }

        if self.active_modal.is_some() {
            if self.active_modal == Some(Modal::ImagePreview) {
                match key.code {
                    KeyCode::Esc => self.close_modal(),
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        self.should_quit = true;
                    }
                    KeyCode::Char('j') | KeyCode::Down | KeyCode::Right | KeyCode::Tab => {
                        self.move_image_preview_next();
                    }
                    KeyCode::Char('k') | KeyCode::Up | KeyCode::Left | KeyCode::BackTab => {
                        self.move_image_preview_previous();
                    }
                    KeyCode::Home => self.move_image_preview_first(),
                    KeyCode::End => self.move_image_preview_last(),
                    KeyCode::Char('+') | KeyCode::Char('=') => self.zoom_image_preview_in(),
                    KeyCode::Char('-') => self.zoom_image_preview_out(),
                    KeyCode::Char('0') => self.reset_image_preview_zoom(),
                    KeyCode::Char('o') => self.open_selected_preview_image_external(),
                    KeyCode::Char('y') => self.copy_selected_preview_image_path(),
                    KeyCode::Char('d') | KeyCode::Delete | KeyCode::Backspace => {
                        self.detach_current_preview_image();
                    }
                    _ => {}
                }
                return;
            }

            if self.active_modal == Some(Modal::Jobs) {
                match key.code {
                    KeyCode::Esc => self.close_modal(),
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        self.should_quit = true
                    }
                    _ => {}
                }
                return;
            }

            if self.active_modal == Some(Modal::Themes) {
                match key.code {
                    KeyCode::Esc => self.close_modal(),
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        self.should_quit = true;
                    }
                    KeyCode::Up | KeyCode::BackTab => self.move_theme_selection_up(),
                    KeyCode::Down | KeyCode::Tab => self.move_theme_selection_down(),
                    KeyCode::Home => {
                        self.theme_selection = 0;
                        self.preview_theme_selection();
                    }
                    KeyCode::End => {
                        self.theme_selection = ThemeKind::all().len().saturating_sub(1);
                        self.preview_theme_selection();
                    }
                    KeyCode::Enter => self.accept_theme_selection(),
                    _ => {}
                }
                return;
            }

            if self.active_modal == Some(Modal::Rewind) {
                self.handle_rewind_key(key);
                return;
            }

            if self.active_modal == Some(Modal::EditMessage) {
                self.handle_edit_message_key(key);
                return;
            }

            if self.active_modal == Some(Modal::Models) {
                match key.code {
                    KeyCode::Esc => self.close_modal(),
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        self.should_quit = true;
                    }
                    KeyCode::Up | KeyCode::BackTab => self.move_model_selection_up(),
                    KeyCode::Down | KeyCode::Tab => self.move_model_selection_down(),
                    KeyCode::Home => self.model_selection = 0,
                    KeyCode::End => {
                        self.model_selection = model_choices(self.model.model_name())
                            .len()
                            .saturating_sub(1);
                    }
                    KeyCode::Enter => self.accept_model_selection(),
                    _ => {}
                }
                return;
            }

            if self.active_modal == Some(Modal::Reasoning) {
                match key.code {
                    KeyCode::Esc => self.close_modal(),
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        self.should_quit = true;
                    }
                    KeyCode::Up | KeyCode::BackTab => self.move_reasoning_selection_up(),
                    KeyCode::Down | KeyCode::Tab => self.move_reasoning_selection_down(),
                    KeyCode::Home => self.reasoning_selection = 0,
                    KeyCode::End => {
                        self.reasoning_selection = reasoning_choices(
                            self.model.model_name(),
                            self.model.reasoning_effort(),
                        )
                        .len()
                        .saturating_sub(1);
                    }
                    KeyCode::Enter => self.accept_reasoning_selection(),
                    _ => {}
                }
                return;
            }

            if self.active_modal == Some(Modal::Permissions) {
                match key.code {
                    KeyCode::Esc => self.close_modal(),
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        self.should_quit = true;
                    }
                    KeyCode::Up | KeyCode::BackTab => self.move_permission_selection_up(),
                    KeyCode::Down | KeyCode::Tab => self.move_permission_selection_down(),
                    KeyCode::Home => self.permission_selection = 0,
                    KeyCode::End => {
                        self.permission_selection = PermissionMode::all().len().saturating_sub(1);
                    }
                    KeyCode::Enter => self.accept_permission_selection(),
                    _ => {}
                }
                return;
            }

            if self.active_modal == Some(Modal::Settings) {
                match key.code {
                    KeyCode::Esc => self.close_modal(),
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        self.should_quit = true;
                    }
                    KeyCode::Up | KeyCode::BackTab => self.move_settings_selection_up(),
                    KeyCode::Down | KeyCode::Tab => self.move_settings_selection_down(),
                    KeyCode::Home => self.settings_selection = 0,
                    KeyCode::End => {
                        self.settings_selection = self.settings_rows().len().saturating_sub(1);
                    }
                    KeyCode::Enter => self.accept_settings_selection(),
                    _ => {}
                }
                return;
            }

            match key.code {
                KeyCode::Esc | KeyCode::Enter => self.close_modal(),
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.should_quit = true;
                }
                _ => {}
            }
            return;
        }

        match key.code {
            KeyCode::Esc if self.slash_suggestions_active() => self.close_command_palette(),
            KeyCode::Esc if self.mention_popup_visible() => self.dismiss_mention_picker(),
            KeyCode::Esc => self.handle_escape(),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true;
            }
            KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.open_command_palette();
            }
            KeyCode::Char('i') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.paste_image_from_clipboard();
            }
            KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.open_latest_image_preview();
            }
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.detach_latest_pending_attachment();
            }
            KeyCode::PageUp if self.slash_suggestions_active() => self.page_slash_selection_up(),
            KeyCode::PageDown if self.slash_suggestions_active() => {
                self.page_slash_selection_down();
            }
            KeyCode::PageUp => self.scroll_chat_up(self.chat_page_scroll_amount()),
            KeyCode::PageDown => self.scroll_chat_down(self.chat_page_scroll_amount()),
            KeyCode::Up if key.modifiers.contains(KeyModifiers::CONTROL) => self.scroll_chat_up(1),
            KeyCode::Down if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.scroll_chat_down(1);
            }
            KeyCode::Home if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.scroll_chat_to_top();
            }
            KeyCode::End if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.scroll_chat_to_bottom();
            }
            _ if self.handle_decision_key(key) => {}
            KeyCode::Up if self.slash_suggestions_active() => self.move_slash_selection_up(),
            KeyCode::Down if self.slash_suggestions_active() => self.move_slash_selection_down(),
            KeyCode::Tab if self.slash_suggestions_active() => self.move_slash_selection_down(),
            KeyCode::BackTab if self.slash_suggestions_active() => self.move_slash_selection_up(),
            KeyCode::Up if self.mention_popup_visible() => self.move_mention_selection_up(),
            KeyCode::Down if self.mention_popup_visible() => self.move_mention_selection_down(),
            KeyCode::Tab if self.mention_popup_visible() => self.accept_mention_suggestion(),
            KeyCode::BackTab if self.mention_popup_visible() => self.move_mention_selection_up(),
            KeyCode::BackTab => self.toggle_plan_mode(),
            KeyCode::Home if key.modifiers.is_empty() && self.slash_suggestions_active() => {
                self.move_slash_selection_first();
            }
            KeyCode::End if key.modifiers.is_empty() && self.slash_suggestions_active() => {
                self.move_slash_selection_last();
            }
            KeyCode::Char('j') if self.input.is_empty() && self.pending_decision().is_none() => {
                self.select_next_tool()
            }
            KeyCode::Char('k') if self.input.is_empty() && self.pending_decision().is_none() => {
                self.select_previous_tool()
            }
            KeyCode::Char('x') if self.input.is_empty() && self.pending_decision().is_none() => {
                self.close_selected_tool()
            }
            KeyCode::Enter if self.input.is_empty() && self.selected_tool.is_some() => {
                self.toggle_selected_tool();
            }
            KeyCode::Enter
                if key
                    .modifiers
                    .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
            {
                self.insert_input_char('\n');
            }
            KeyCode::Enter | KeyCode::Char('\n') | KeyCode::Char('\r')
                if self.slash_suggestions_active() =>
            {
                self.accept_slash_suggestion();
            }
            KeyCode::Enter | KeyCode::Char('\n') | KeyCode::Char('\r')
                if self.mention_popup_visible() =>
            {
                self.accept_mention_suggestion();
            }
            KeyCode::Enter | KeyCode::Char('\n') | KeyCode::Char('\r') => self.submit_input(),
            KeyCode::Char('j' | 'm') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.submit_input();
            }
            KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.input_cursor = self.input_len();
            }
            KeyCode::Left => self.move_input_cursor_left(),
            KeyCode::Right => self.move_input_cursor_right(),
            KeyCode::Up if !self.input.is_empty() && self.input.contains('\n') => {
                self.move_input_cursor_vertical(-1);
            }
            KeyCode::Down if !self.input.is_empty() && self.input.contains('\n') => {
                self.move_input_cursor_vertical(1);
            }
            KeyCode::Home if key.modifiers.is_empty() => {
                self.input_cursor = self.input_current_line_bounds().0;
            }
            KeyCode::End if key.modifiers.is_empty() => {
                self.input_cursor = self.input_current_line_bounds().1;
            }
            KeyCode::Delete => self.delete_input_char(),
            KeyCode::Char(_)
                if key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.status_line = "Unsupported shortcut.".to_string();
            }
            KeyCode::Char(ch) => self.insert_input_char(ch),
            KeyCode::Backspace => self.backspace_input_char(),
            _ => {}
        }
    }

    fn input_len(&self) -> usize {
        self.input.chars().count()
    }

    /// Char-index bounds (start, end) of the line the cursor is on,
    /// excluding the trailing newline.
    fn input_current_line_bounds(&self) -> (usize, usize) {
        let mut start = 0;
        let mut index = 0;
        for ch in self.input.chars() {
            if ch == '\n' {
                if index >= self.input_cursor {
                    return (start, index);
                }
                start = index + 1;
            }
            index += 1;
        }
        (start, index)
    }

    /// Move the cursor up/down one visual input line, keeping the column
    /// where possible (clamped to the target line's length).
    fn move_input_cursor_vertical(&mut self, delta: isize) {
        let lines: Vec<&str> = self.input.split('\n').collect();
        // Locate the cursor's (line, column).
        let mut remaining = self.input_cursor;
        let mut line_index = 0;
        for (index, line) in lines.iter().enumerate() {
            let len = line.chars().count();
            if remaining <= len {
                line_index = index;
                break;
            }
            remaining -= len + 1;
            line_index = index;
        }
        let column = remaining;

        let target = line_index.saturating_add_signed(delta);
        if target >= lines.len() || target == line_index {
            return;
        }

        let target_column = column.min(lines[target].chars().count());
        let mut cursor = 0;
        for line in lines.iter().take(target) {
            cursor += line.chars().count() + 1;
        }
        self.input_cursor = cursor + target_column;
    }

    fn input_byte_index(&self, char_index: usize) -> usize {
        self.input
            .char_indices()
            .nth(char_index)
            .map(|(index, _)| index)
            .unwrap_or(self.input.len())
    }

    fn insert_input_char(&mut self, ch: char) {
        let index = self.input_byte_index(self.input_cursor);
        self.input.insert(index, ch);
        self.input_cursor += 1;
        self.clamp_slash_selection();
        self.refresh_mention_state();
    }

    fn backspace_input_char(&mut self) {
        if self.input_cursor == 0 {
            self.clamp_slash_selection();
            return;
        }

        let start = self.input_byte_index(self.input_cursor - 1);
        let end = self.input_byte_index(self.input_cursor);
        self.input.replace_range(start..end, "");
        self.input_cursor -= 1;
        self.clamp_slash_selection();
        self.refresh_mention_state();
    }

    fn delete_input_char(&mut self) {
        if self.input_cursor >= self.input_len() {
            self.clamp_slash_selection();
            return;
        }

        let start = self.input_byte_index(self.input_cursor);
        let end = self.input_byte_index(self.input_cursor + 1);
        self.input.replace_range(start..end, "");
        self.clamp_slash_selection();
        self.refresh_mention_state();
    }

    fn handle_paste(&mut self, text: String) {
        if let Some(path) = single_image_path(&text) {
            match self.attach_image_path(&path) {
                Ok(()) => return,
                Err(error) => {
                    self.status_line = format!("image attach failed: {error}");
                    self.toast("Image attach failed", ToastKind::Error);
                    return;
                }
            }
        }

        let index = self.input_byte_index(self.input_cursor);
        let pasted_chars = text.chars().count();
        self.input.insert_str(index, &text);
        self.input_cursor += pasted_chars;
        self.clamp_slash_selection();
        self.refresh_mention_state();
    }

    fn paste_image_from_clipboard(&mut self) {
        match self.read_clipboard_image() {
            Ok(attachment) => {
                let label = attachment_label(&attachment);
                self.cache_attachment_preview(&attachment);
                self.pending_attachments.push(attachment);
                self.status_line = format!("attached {label}");
                self.toast("Image attached", ToastKind::Success);
            }
            Err(error) => {
                self.status_line = format!("clipboard image unavailable: {error}");
                self.toast("No clipboard image", ToastKind::Warning);
            }
        }
    }

    fn read_clipboard_image(&self) -> Result<ImageAttachment> {
        let mut clipboard = Clipboard::new().wrap_err("failed to open clipboard")?;
        let image = clipboard
            .get_image()
            .wrap_err("clipboard does not contain an image")?;
        let width = image.width as u32;
        let height = image.height as u32;
        let rgba = image.bytes.into_owned();
        let mut png = Vec::new();
        PngEncoder::new(&mut png)
            .write_image(&rgba, width, height, ColorType::Rgba8.into())
            .wrap_err("failed to encode clipboard image")?;
        self.store_image_bytes("clipboard", "image/png", width, height, png)
    }

    fn attach_image_path(&mut self, path: &Path) -> Result<()> {
        let bytes =
            fs::read(path).wrap_err_with(|| format!("failed to read {}", path.display()))?;
        let image = image::load_from_memory(&bytes)
            .wrap_err_with(|| format!("failed to decode image {}", path.display()))?;
        let mime = image_mime_from_path(path).unwrap_or("image/png");
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("image");
        let attachment =
            self.store_image_bytes(name, mime, image.width(), image.height(), bytes)?;
        let label = attachment_label(&attachment);
        self.cache_attachment_preview(&attachment);
        self.pending_attachments.push(attachment);
        self.status_line = format!("attached {label}");
        self.toast("Image attached", ToastKind::Success);
        Ok(())
    }

    fn cache_attachment_preview(&mut self, attachment: &ImageAttachment) {
        self.attachment_previews.insert(
            attachment.id.clone(),
            image_preview_lines(attachment, COMPOSER_IMAGE_PREVIEW_WIDTH),
        );
    }

    fn store_image_bytes(
        &self,
        name_hint: &str,
        mime: &str,
        width: u32,
        height: u32,
        bytes: Vec<u8>,
    ) -> Result<ImageAttachment> {
        let id = format!("image-{}", attachment_timestamp());
        let extension = image_extension(mime);
        let safe_name = sanitize_attachment_name(name_hint, extension);
        let file_name = format!("{id}-{safe_name}");
        let dir = self.attachment_dir()?;
        fs::create_dir_all(&dir).wrap_err("failed to create attachments directory")?;
        let path = dir.join(file_name);
        fs::write(&path, &bytes).wrap_err_with(|| format!("failed to write {}", path.display()))?;
        Ok(ImageAttachment {
            id,
            name: safe_name,
            path,
            mime: mime.to_string(),
            width,
            height,
            size_bytes: bytes.len() as u64,
        })
    }

    fn attachment_dir(&self) -> Result<PathBuf> {
        if let Some(session) = &self.session {
            return Ok(session.attachment_dir());
        }
        Ok(env::current_dir()?.join(".medusa").join("attachments"))
    }

    fn image_attachments(&self) -> Vec<ImageAttachment> {
        let mut attachments = Vec::new();
        for item in &self.transcript {
            if let TranscriptItem::Message(message) = item {
                attachments.extend(message.attachments.iter().cloned());
            }
        }
        attachments.extend(self.pending_attachments.iter().cloned());
        attachments
    }

    fn current_preview_image(&self) -> Option<ImageAttachment> {
        let attachments = self.image_attachments();
        if attachments.is_empty() {
            return None;
        }
        attachments
            .get(
                self.image_preview_index
                    .min(attachments.len().saturating_sub(1)),
            )
            .cloned()
    }

    fn current_preview_image_is_pending(&self) -> bool {
        self.current_preview_image().is_some_and(|attachment| {
            self.pending_attachments
                .iter()
                .any(|pending| pending.id == attachment.id)
        })
    }

    fn open_image_preview(&mut self, index: usize) {
        let count = self.image_attachments().len();
        if count == 0 {
            self.status_line = "no images to preview".to_string();
            self.toast("No images attached", ToastKind::Info);
            return;
        }

        self.image_preview_index = index.min(count.saturating_sub(1));
        self.image_preview_zoom = self
            .image_preview_zoom
            .clamp(IMAGE_PREVIEW_MIN_ZOOM, IMAGE_PREVIEW_MAX_ZOOM);
        self.active_modal = Some(Modal::ImagePreview);
        self.status_line = format!("image preview {}/{}", self.image_preview_index + 1, count);
    }

    fn open_latest_image_preview(&mut self) {
        let count = self.image_attachments().len();
        if count == 0 {
            self.status_line = "no images to preview".to_string();
            self.toast("No images attached", ToastKind::Info);
            return;
        }
        self.open_image_preview(count.saturating_sub(1));
    }

    fn open_image_preview_for_attachment(&mut self, attachment: &ImageAttachment) {
        let attachments = self.image_attachments();
        let index = attachments
            .iter()
            .position(|candidate| candidate.id == attachment.id)
            .unwrap_or_else(|| attachments.len().saturating_sub(1));
        self.open_image_preview(index);
    }

    fn move_image_preview_next(&mut self) {
        let count = self.image_attachments().len();
        if count == 0 {
            return;
        }
        self.image_preview_index = (self.image_preview_index + 1) % count;
        self.status_line = format!("image preview {}/{}", self.image_preview_index + 1, count);
    }

    fn move_image_preview_previous(&mut self) {
        let count = self.image_attachments().len();
        if count == 0 {
            return;
        }
        self.image_preview_index = if self.image_preview_index == 0 {
            count - 1
        } else {
            self.image_preview_index - 1
        };
        self.status_line = format!("image preview {}/{}", self.image_preview_index + 1, count);
    }

    fn move_image_preview_first(&mut self) {
        if !self.image_attachments().is_empty() {
            self.image_preview_index = 0;
            self.status_line = "first image".to_string();
        }
    }

    fn move_image_preview_last(&mut self) {
        let count = self.image_attachments().len();
        if count > 0 {
            self.image_preview_index = count - 1;
            self.status_line = "last image".to_string();
        }
    }

    fn zoom_image_preview_in(&mut self) {
        self.image_preview_zoom = self
            .image_preview_zoom
            .saturating_add(IMAGE_PREVIEW_ZOOM_STEP)
            .min(IMAGE_PREVIEW_MAX_ZOOM);
        self.status_line = format!("image zoom {}%", self.image_preview_zoom);
    }

    fn zoom_image_preview_out(&mut self) {
        self.image_preview_zoom = self
            .image_preview_zoom
            .saturating_sub(IMAGE_PREVIEW_ZOOM_STEP)
            .max(IMAGE_PREVIEW_MIN_ZOOM);
        self.status_line = format!("image zoom {}%", self.image_preview_zoom);
    }

    fn reset_image_preview_zoom(&mut self) {
        self.image_preview_zoom = 100;
        self.status_line = "image zoom reset".to_string();
    }

    fn open_selected_preview_image_external(&mut self) {
        let Some(attachment) = self.current_preview_image() else {
            self.status_line = "no image selected".to_string();
            return;
        };

        let result = if cfg!(target_os = "macos") {
            Command::new("open").arg(&attachment.path).spawn()
        } else if cfg!(target_os = "windows") {
            Command::new("cmd")
                .args(["/C", "start", ""])
                .arg(&attachment.path)
                .spawn()
        } else {
            Command::new("xdg-open").arg(&attachment.path).spawn()
        };

        match result {
            Ok(_) => {
                self.status_line = format!("opened {}", attachment.name);
                self.toast("Image opened", ToastKind::Success);
            }
            Err(error) => {
                self.status_line = format!("open image failed: {error}");
                self.toast("Open image failed", ToastKind::Error);
            }
        }
    }

    fn copy_selected_preview_image_path(&mut self) {
        let Some(attachment) = self.current_preview_image() else {
            self.status_line = "no image selected".to_string();
            return;
        };

        let path = attachment.path.to_string_lossy().to_string();
        match Clipboard::new().and_then(|mut clipboard| clipboard.set_text(path.clone())) {
            Ok(()) => {
                self.status_line = "image path copied".to_string();
                self.toast("Image path copied", ToastKind::Success);
            }
            Err(error) => {
                self.status_line = format!("copy path failed: {error}");
                self.toast("Copy path failed", ToastKind::Error);
            }
        }
    }

    fn detach_latest_pending_attachment(&mut self) {
        let Some(index) = self.pending_attachments.len().checked_sub(1) else {
            self.status_line = "no pending image to detach".to_string();
            self.toast("No pending image", ToastKind::Info);
            return;
        };
        self.detach_pending_attachment_at(index);
        self.status_line = "detached latest image".to_string();
        self.toast("Image detached", ToastKind::Success);
    }

    fn detach_current_preview_image(&mut self) {
        let Some(attachment) = self.current_preview_image() else {
            self.status_line = "no image selected".to_string();
            return;
        };
        let Some(index) = self
            .pending_attachments
            .iter()
            .position(|pending| pending.id == attachment.id)
        else {
            self.status_line = "sent image stays in transcript".to_string();
            self.toast("Only pending images can be detached", ToastKind::Warning);
            return;
        };

        self.detach_pending_attachment_at(index);
        let remaining = self.image_attachments().len();
        if remaining == 0 {
            self.active_modal = None;
            self.image_preview_index = 0;
            self.status_line = "image detached".to_string();
        } else {
            self.image_preview_index = self.image_preview_index.min(remaining.saturating_sub(1));
            self.status_line = format!(
                "image detached · preview {}/{}",
                self.image_preview_index + 1,
                remaining
            );
        }
        self.toast("Image detached", ToastKind::Success);
    }

    fn detach_pending_attachment_at(&mut self, index: usize) -> Option<ImageAttachment> {
        if index >= self.pending_attachments.len() {
            return None;
        }
        let attachment = self.pending_attachments.remove(index);
        self.attachment_previews.remove(&attachment.id);
        self.image_renderer.forget(&attachment.id);
        let _ = fs::remove_file(&attachment.path);
        Some(attachment)
    }

    fn move_input_cursor_left(&mut self) {
        self.input_cursor = self.input_cursor.saturating_sub(1);
    }

    fn move_input_cursor_right(&mut self) {
        self.input_cursor = (self.input_cursor + 1).min(self.input_len());
    }

    fn open_command_palette(&mut self) {
        self.input = "/".to_string();
        self.input_cursor = self.input_len();
        self.slash_selection = 0;
        self.status_line = "command palette".to_string();
    }

    fn close_command_palette(&mut self) {
        self.input.clear();
        self.input_cursor = 0;
        self.slash_selection = 0;
        self.status_line = "command palette closed".to_string();
    }

    /// Contextual Esc: clear what's in the way first; only a second quick
    /// Esc on an idle composer quits, so a stray keypress can't kill the
    /// session.
    fn handle_escape(&mut self) {
        if self.selected_tool.is_some() {
            self.close_selected_tool();
            return;
        }
        if !self.input.is_empty() {
            self.input.clear();
            self.input_cursor = 0;
            self.last_escape_at = None;
            self.status_line = "input cleared".to_string();
            return;
        }
        if self.is_working() {
            // Never arms double-esc quit: cancelling a turn and quitting the
            // app must stay two distinct gestures.
            self.last_escape_at = None;
            if self.cancel_requested_at.is_some() {
                self.force_abandon_turn();
            } else {
                self.request_cancel_turn();
            }
            return;
        }
        if self.has_active_workflows() {
            // A background workflow is running but no model turn is streaming
            // (`is_working()` is false). Esc cancels the workflow — never falls
            // through to the double-esc quit arm, which would kill subagents
            // mid file_edit/file_patch and orphan their process-grouped
            // children. Like a turn cancel, this never arms quit.
            self.last_escape_at = None;
            self.cancel_active_workflows();
            return;
        }
        if self.plan_mode {
            self.toggle_plan_mode();
            self.last_escape_at = None;
            return;
        }
        if self
            .last_escape_at
            .is_some_and(|at| at.elapsed() <= DOUBLE_ESCAPE_WINDOW)
        {
            self.should_quit = true;
            return;
        }
        self.last_escape_at = Some(Instant::now());
        self.status_line = "press esc again to quit".to_string();
    }

    fn toggle_plan_mode(&mut self) {
        self.plan_mode = !self.plan_mode;
        if self.plan_mode {
            self.status_line = "plan mode on · read-only exploration".to_string();
            self.toast(
                "Plan mode on — model will plan before editing",
                ToastKind::Info,
            );
        } else {
            self.status_line = "plan mode off".to_string();
            self.toast("Plan mode off — edits allowed", ToastKind::Info);
        }
    }

    fn close_modal(&mut self) {
        if self.active_modal == Some(Modal::Themes) {
            self.cancel_theme_preview();
        }
        self.active_modal = None;
        self.status_line = "closed".to_string();
    }

    fn slash_suggestions_active(&self) -> bool {
        let input = self.input.trim_start();
        input.starts_with('/')
            && (!input.contains(char::is_whitespace) || input.starts_with("/theme "))
    }

    fn slash_matches(&self) -> Vec<(&'static SlashCommand, Vec<usize>)> {
        if !self.slash_suggestions_active() {
            return Vec::new();
        }

        let input = self.input.trim_start();
        let (commands, query) = if let Some(theme_query) = input.strip_prefix("/theme ") {
            (
                THEME_SLASH_COMMANDS,
                theme_query.trim().to_ascii_lowercase(),
            )
        } else {
            (
                SLASH_COMMANDS,
                input.trim_start_matches('/').trim().to_ascii_lowercase(),
            )
        };
        let mut matches = commands
            .iter()
            .enumerate()
            .filter_map(|(index, command)| {
                slash_match(command, &query)
                    .map(|(score, positions)| (score, index, command, positions))
            })
            .collect::<Vec<_>>();
        matches.sort_by_key(|(score, index, _, _)| (*score, *index));
        matches
            .into_iter()
            .map(|(_, _, command, positions)| (command, positions))
            .collect()
    }

    fn clamp_slash_selection(&mut self) {
        let count = self.slash_matches().len();
        if count == 0 {
            self.slash_selection = 0;
        } else if self.slash_selection >= count {
            self.slash_selection = count - 1;
        }
    }

    fn move_slash_selection_up(&mut self) {
        let count = self.slash_matches().len();
        if count == 0 {
            return;
        }
        self.slash_selection = if self.slash_selection == 0 {
            count - 1
        } else {
            self.slash_selection - 1
        };
        self.status_line = "command suggestion".to_string();
    }

    fn move_slash_selection_down(&mut self) {
        let count = self.slash_matches().len();
        if count == 0 {
            return;
        }
        self.slash_selection = (self.slash_selection + 1) % count;
        self.status_line = "command suggestion".to_string();
    }

    fn page_slash_selection_up(&mut self) {
        self.move_slash_selection_by(-6);
    }

    fn page_slash_selection_down(&mut self) {
        self.move_slash_selection_by(6);
    }

    fn move_slash_selection_by(&mut self, amount: isize) {
        let count = self.slash_matches().len();
        if count == 0 {
            return;
        }

        let max = count.saturating_sub(1) as isize;
        let next = (self.slash_selection as isize + amount).clamp(0, max);
        self.slash_selection = next as usize;
        self.status_line = "command suggestion".to_string();
    }

    fn move_slash_selection_first(&mut self) {
        if !self.slash_matches().is_empty() {
            self.slash_selection = 0;
            self.status_line = "command suggestion".to_string();
        }
    }

    fn move_slash_selection_last(&mut self) {
        let count = self.slash_matches().len();
        if count > 0 {
            self.slash_selection = count - 1;
            self.status_line = "command suggestion".to_string();
        }
    }

    fn accept_slash_suggestion(&mut self) {
        let Some(command) = self
            .slash_matches()
            .get(self.slash_selection)
            .map(|(command, _)| *command)
        else {
            self.submit_input();
            return;
        };

        // Typing a command out in full and hitting Enter runs it as typed
        // instead of re-completing it into the composer.
        if self.input.trim() == command.name
            && !matches!(
                command.name,
                "/theme" | "/model" | "/reasoning" | "/permissions"
            )
        {
            self.submit_input();
            return;
        }

        if command.name == "/theme" {
            self.input.clear();
            self.input_cursor = 0;
            self.open_themes_modal();
        } else if command.name == "/model" {
            self.input.clear();
            self.input_cursor = 0;
            self.open_models_modal();
        } else if command.name == "/reasoning" {
            self.input.clear();
            self.input_cursor = 0;
            self.open_reasoning_modal();
        } else if command.name == "/permissions" {
            self.input.clear();
            self.input_cursor = 0;
            self.open_permissions_modal();
        } else if command.args.is_empty() {
            self.input = command.name.to_string();
            self.input_cursor = self.input_len();
            self.submit_input();
        } else {
            self.input = format!("{} ", command.name);
            self.input_cursor = self.input_len();
            self.status_line = format!("{} needs {}", command.name, command.args);
        }
    }

    /// Char-index span (start..end) and typed query of the @token under the
    /// cursor: an '@' at a token boundary (start of input or after
    /// whitespace) with no whitespace between it and the cursor. `end`
    /// extends to the end of the contiguous token so accepting a suggestion
    /// mid-token replaces the whole token; the query is only what was typed
    /// so far (between '@' and the cursor).
    fn active_mention_token(&self) -> Option<(usize, usize, String)> {
        let chars: Vec<char> = self.input.chars().collect();
        let cursor = self.input_cursor.min(chars.len());
        let mut at = None;
        for index in (0..cursor).rev() {
            let ch = chars[index];
            if ch == '@' {
                if index == 0 || chars[index - 1].is_whitespace() {
                    at = Some(index);
                }
                break;
            }
            if ch.is_whitespace() {
                break;
            }
        }
        let start = at?;
        let mut end = cursor;
        while end < chars.len() && !chars[end].is_whitespace() {
            end += 1;
        }
        let query = chars[start + 1..cursor].iter().collect();
        Some((start, end, query))
    }

    fn mention_active(&self) -> bool {
        !self.mention_dismissed
            && !self.slash_suggestions_active()
            && self.active_mention_token().is_some()
    }

    fn mention_popup_visible(&self) -> bool {
        self.mention_active() && !self.mention_matches().is_empty()
    }

    /// Keep mention picker state in sync after any composer edit: load the
    /// workspace file list when an @token appears, drop it when the token
    /// goes away, and clear an Esc dismissal (typing reopens the picker).
    fn refresh_mention_state(&mut self) {
        self.mention_dismissed = false;
        if !self.slash_suggestions_active() && self.active_mention_token().is_some() {
            if self.mention_files.is_none() {
                self.mention_files = Some(collect_workspace_files(
                    self.tools.workspace(),
                    MENTION_FILE_WALK_CAP,
                ));
            }
            self.clamp_mention_selection();
        } else {
            self.mention_files = None;
            self.mention_selection = 0;
        }
    }

    fn mention_matches(&self) -> Vec<(&str, Vec<usize>)> {
        if !self.mention_active() {
            return Vec::new();
        }
        let Some((_, _, query)) = self.active_mention_token() else {
            return Vec::new();
        };
        let Some(files) = &self.mention_files else {
            return Vec::new();
        };

        let query = query.to_ascii_lowercase();
        let mut matches = files
            .iter()
            .enumerate()
            .filter_map(|(index, path)| {
                mention_match(path, &query)
                    .map(|(score, positions)| (score, index, path.as_str(), positions))
            })
            .collect::<Vec<_>>();
        matches.sort_by_key(|(score, index, _, _)| (*score, *index));
        matches.truncate(MENTION_MATCH_LIMIT);
        matches
            .into_iter()
            .map(|(_, _, path, positions)| (path, positions))
            .collect()
    }

    fn clamp_mention_selection(&mut self) {
        let count = self.mention_matches().len();
        if count == 0 {
            self.mention_selection = 0;
        } else if self.mention_selection >= count {
            self.mention_selection = count - 1;
        }
    }

    fn move_mention_selection_up(&mut self) {
        let count = self.mention_matches().len();
        if count == 0 {
            return;
        }
        self.mention_selection = if self.mention_selection == 0 {
            count - 1
        } else {
            self.mention_selection - 1
        };
        self.status_line = "file suggestion".to_string();
    }

    fn move_mention_selection_down(&mut self) {
        let count = self.mention_matches().len();
        if count == 0 {
            return;
        }
        self.mention_selection = (self.mention_selection + 1) % count;
        self.status_line = "file suggestion".to_string();
    }

    /// Replace the @token with the selected workspace-relative path (plain
    /// text — the model reads files itself) plus a trailing space.
    fn accept_mention_suggestion(&mut self) {
        let Some(path) = self
            .mention_matches()
            .get(self.mention_selection)
            .map(|(path, _)| (*path).to_string())
        else {
            return;
        };
        let Some((start, end, _)) = self.active_mention_token() else {
            return;
        };

        let start_byte = self.input_byte_index(start);
        let end_byte = self.input_byte_index(end);
        self.input
            .replace_range(start_byte..end_byte, &format!("{path} "));
        self.input_cursor = start + path.chars().count() + 1;
        self.mention_selection = 0;
        self.mention_files = None;
        self.status_line = format!("mentioned {path}");
    }

    fn dismiss_mention_picker(&mut self) {
        self.mention_dismissed = true;
        self.mention_selection = 0;
        self.status_line = "file picker closed".to_string();
    }

    fn open_settings_modal(&mut self) {
        self.active_modal = Some(Modal::Settings);
        self.settings_selection = 0;
        self.status_line = "settings opened".to_string();
    }

    fn open_models_modal(&mut self) {
        self.active_modal = Some(Modal::Models);
        self.model_selection = model_index(self.model.model_name());
        self.status_line = "models opened".to_string();
    }

    fn open_reasoning_modal(&mut self) {
        self.active_modal = Some(Modal::Reasoning);
        self.reasoning_selection =
            reasoning_index(self.model.model_name(), self.model.reasoning_effort());
        self.status_line = "reasoning effort opened".to_string();
    }

    fn open_permissions_modal(&mut self) {
        self.active_modal = Some(Modal::Permissions);
        self.permission_selection = permission_mode_index(self.permission_mode);
        self.status_line = "permissions opened".to_string();
    }

    fn open_themes_modal(&mut self) {
        self.active_modal = Some(Modal::Themes);
        self.theme_preview_original.get_or_insert(self.theme);
        self.theme_selection = theme_index(self.theme);
        self.status_line = "themes opened".to_string();
    }

    fn move_settings_selection_up(&mut self) {
        let count = self.settings_rows().len();
        if count == 0 {
            return;
        }
        self.settings_selection = if self.settings_selection == 0 {
            count - 1
        } else {
            self.settings_selection - 1
        };
        self.status_line = "settings selection".to_string();
    }

    fn move_settings_selection_down(&mut self) {
        let count = self.settings_rows().len();
        if count == 0 {
            return;
        }
        self.settings_selection = (self.settings_selection + 1) % count;
        self.status_line = "settings selection".to_string();
    }

    fn accept_settings_selection(&mut self) {
        let items = self.settings_items();
        match items.get(self.settings_selection).map(|item| item.key) {
            Some("model") => self.open_models_modal(),
            Some("theme") => self.open_themes_modal(),
            Some("permissions") => self.open_permissions_modal(),
            Some("bell") => self.toggle_bell_setting(),
            Some(_) | None => {
                self.status_line = "setting is read-only".to_string();
                self.toast("Read-only setting", ToastKind::Info);
            }
        }
    }

    fn toggle_bell_setting(&mut self) {
        self.bell_setting = !self.bell_setting;
        let label = if self.bell_setting {
            "Bell on"
        } else {
            "Bell off"
        };
        match save_bell_preference(self.tools.workspace(), self.bell_setting) {
            Ok(()) => {
                self.status_line = label.to_ascii_lowercase();
                self.toast(label, ToastKind::Info);
            }
            Err(error) => {
                self.status_line = format!("bell preference not saved: {error}");
                self.toast("Bell preference not saved", ToastKind::Warning);
            }
        }
    }

    fn move_model_selection_up(&mut self) {
        let count = model_choices(self.model.model_name()).len();
        if count == 0 {
            return;
        }
        self.model_selection = if self.model_selection == 0 {
            count - 1
        } else {
            self.model_selection - 1
        };
        self.status_line = "model selection".to_string();
    }

    fn move_model_selection_down(&mut self) {
        let count = model_choices(self.model.model_name()).len();
        if count == 0 {
            return;
        }
        self.model_selection = (self.model_selection + 1) % count;
        self.status_line = "model selection".to_string();
    }

    fn accept_model_selection(&mut self) {
        let choices = model_choices(self.model.model_name());
        let Some(model) = choices
            .get(self.model_selection.min(choices.len().saturating_sub(1)))
            .cloned()
        else {
            return;
        };
        self.set_model_name(&model);
        self.active_modal = None;
    }

    fn move_reasoning_selection_up(&mut self) {
        let count = reasoning_choices(self.model.model_name(), self.model.reasoning_effort()).len();
        if count == 0 {
            return;
        }
        self.reasoning_selection = if self.reasoning_selection == 0 {
            count - 1
        } else {
            self.reasoning_selection - 1
        };
        self.status_line = "reasoning selection".to_string();
    }

    fn move_reasoning_selection_down(&mut self) {
        let count = reasoning_choices(self.model.model_name(), self.model.reasoning_effort()).len();
        if count == 0 {
            return;
        }
        self.reasoning_selection = (self.reasoning_selection + 1) % count;
        self.status_line = "reasoning selection".to_string();
    }

    fn accept_reasoning_selection(&mut self) {
        let choices = reasoning_choices(self.model.model_name(), self.model.reasoning_effort());
        let Some(effort) = choices
            .get(
                self.reasoning_selection
                    .min(choices.len().saturating_sub(1)),
            )
            .cloned()
        else {
            return;
        };
        self.set_reasoning_effort(&effort);
        self.active_modal = None;
    }

    fn move_permission_selection_up(&mut self) {
        let count = PermissionMode::all().len();
        if count == 0 {
            return;
        }
        self.permission_selection = if self.permission_selection == 0 {
            count - 1
        } else {
            self.permission_selection - 1
        };
        self.status_line = "permission selection".to_string();
    }

    fn move_permission_selection_down(&mut self) {
        let count = PermissionMode::all().len();
        if count == 0 {
            return;
        }
        self.permission_selection = (self.permission_selection + 1) % count;
        self.status_line = "permission selection".to_string();
    }

    fn accept_permission_selection(&mut self) {
        let mode = PermissionMode::all()[self
            .permission_selection
            .min(PermissionMode::all().len().saturating_sub(1))];
        self.set_permission_mode(mode);
        self.active_modal = None;
    }

    fn move_theme_selection_up(&mut self) {
        let count = ThemeKind::all().len();
        self.theme_selection = if self.theme_selection == 0 {
            count - 1
        } else {
            self.theme_selection - 1
        };
        self.preview_theme_selection();
    }

    fn move_theme_selection_down(&mut self) {
        let count = ThemeKind::all().len();
        self.theme_selection = (self.theme_selection + 1) % count;
        self.preview_theme_selection();
    }

    fn preview_theme_selection(&mut self) {
        let theme = ThemeKind::all()[self.theme_selection.min(ThemeKind::all().len() - 1)];
        self.theme = theme;
        set_active_theme(theme);
        self.invalidate_render_cache();
        self.status_line = format!("preview theme: {}", theme.name());
    }

    fn accept_theme_selection(&mut self) {
        let theme = ThemeKind::all()[self.theme_selection.min(ThemeKind::all().len() - 1)];
        self.theme_preview_original = None;
        self.set_theme(theme);
        self.active_modal = None;
    }

    fn cancel_theme_preview(&mut self) {
        let Some(theme) = self.theme_preview_original.take() else {
            return;
        };
        self.theme = theme;
        self.theme_selection = theme_index(theme);
        set_active_theme(theme);
        self.invalidate_render_cache();
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) {
        if self.active_modal == Some(Modal::ImagePreview) {
            match mouse.kind {
                MouseEventKind::ScrollUp => self.move_image_preview_previous(),
                MouseEventKind::ScrollDown => self.move_image_preview_next(),
                MouseEventKind::Down(MouseButton::Left) => {
                    self.status_line = "image preview".to_string()
                }
                _ => {}
            }
            return;
        }

        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(attachment) = self.image_attachment_at_mouse(mouse) {
                    self.open_image_preview_for_attachment(&attachment);
                }
            }
            MouseEventKind::ScrollUp => self.scroll_chat_up(self.mouse_scroll_amount(mouse)),
            MouseEventKind::ScrollDown => self.scroll_chat_down(self.mouse_scroll_amount(mouse)),
            _ => {}
        }
    }

    fn mouse_scroll_amount(&self, mouse: MouseEvent) -> usize {
        if mouse.modifiers.contains(KeyModifiers::SHIFT) {
            self.chat_page_scroll_amount()
        } else if mouse.modifiers.contains(KeyModifiers::CONTROL) {
            1
        } else {
            6
        }
    }

    fn image_attachment_at_mouse(&self, mouse: MouseEvent) -> Option<ImageAttachment> {
        let area = self.last_chat_viewport?;
        let rows;
        let rows_ref = if self.last_transcript_rows.is_empty() {
            rows = self.visible_transcript_rows();
            &rows
        } else {
            &self.last_transcript_rows
        };
        if rows_ref.is_empty() {
            return None;
        }

        let metrics = chat_viewport_metrics(rows_ref, area, self.chat_scroll);
        let text_area = metrics.text_area;
        let text_right = text_area.x.saturating_add(text_area.width);
        let text_bottom = text_area.y.saturating_add(text_area.height);
        if mouse.column < text_area.x
            || mouse.column >= text_right
            || mouse.row < text_area.y
            || mouse.row >= text_bottom
        {
            return None;
        }

        for placement in transcript_image_placements(rows_ref, text_area, metrics.top_offset) {
            let x0 = text_area.x.saturating_add(placement.x_offset);
            let x1 = x0.saturating_add(placement.width).min(text_right);
            let y0_raw = text_area.y as i32 + placement.y_offset as i32;
            let y1_raw = y0_raw + placement.height as i32;
            let y0 = y0_raw.max(text_area.y as i32);
            let y1 = y1_raw.min(text_bottom as i32);

            if x0 < x1
                && y0 < y1
                && mouse.column >= x0
                && mouse.column < x1
                && (mouse.row as i32) >= y0
                && (mouse.row as i32) < y1
            {
                return Some(placement.attachment);
            }
        }

        None
    }

    fn scroll_chat_up(&mut self, amount: usize) {
        self.chat_scroll = self.chat_scroll.saturating_add(amount);
        self.chat_scroll_target = self.chat_scroll;
        self.clamp_chat_scroll_to_viewport();
        self.status_line = self.scroll_status_text();
    }

    fn scroll_chat_down(&mut self, amount: usize) {
        self.chat_scroll = self.chat_scroll.saturating_sub(amount);
        self.chat_scroll_target = self.chat_scroll;
        self.clamp_chat_scroll_to_viewport();
        self.status_line = self.scroll_status_text();
    }

    fn scroll_chat_to_top(&mut self) {
        self.chat_scroll = self
            .current_chat_viewport_metrics_for_scroll(self.chat_scroll_target)
            .map_or(usize::MAX / 2, |metrics| metrics.max_scroll);
        self.chat_scroll_target = self.chat_scroll;
        self.status_line = "top".to_string();
    }

    fn scroll_chat_to_bottom(&mut self) {
        self.chat_scroll = 0;
        self.chat_scroll_target = 0;
    }

    fn scroll_status_text(&self) -> String {
        let Some(metrics) = self.current_chat_viewport_metrics_for_scroll(self.chat_scroll_target)
        else {
            return "bottom".to_string();
        };
        if metrics.max_scroll == 0 || self.chat_scroll_target == 0 {
            return "bottom".to_string();
        }
        if self.chat_scroll_target >= metrics.max_scroll {
            return "top".to_string();
        }

        let progress = scroll_progress_percent(&metrics);
        format!("scroll {progress}% · ctrl+end bottom")
    }

    fn clamp_chat_scroll_to_viewport(&mut self) {
        if let Some(metrics) =
            self.current_chat_viewport_metrics_for_scroll(self.chat_scroll_target)
        {
            self.chat_scroll_target = self.chat_scroll_target.min(metrics.max_scroll);
            self.chat_scroll = self.chat_scroll.min(metrics.max_scroll);
        }
    }

    fn chat_page_scroll_amount(&self) -> usize {
        self.last_chat_viewport
            .map(|area| area.height.saturating_sub(2).max(1) as usize)
            .unwrap_or(12)
    }

    fn current_chat_viewport_metrics(&self) -> Option<ChatViewportMetrics> {
        self.current_chat_viewport_metrics_for_scroll(self.chat_scroll)
    }

    fn current_chat_viewport_metrics_for_scroll(
        &self,
        scroll: usize,
    ) -> Option<ChatViewportMetrics> {
        let area = self.last_chat_viewport?;
        if self.last_transcript_rows.is_empty() {
            let rows = self.visible_transcript_rows();
            return Some(chat_viewport_metrics(&rows, area, scroll));
        }
        Some(chat_viewport_metrics(
            &self.last_transcript_rows,
            area,
            scroll,
        ))
    }

    fn stick_chat_to_bottom_if_needed(&mut self) {
        if self.chat_scroll == 0 && self.chat_scroll_target == 0 {
            self.scroll_chat_to_bottom();
        } else {
            self.clamp_chat_scroll_to_viewport();
        }
    }

    fn select_next_tool(&mut self) {
        let tools = self.tool_group_indices();
        if tools.is_empty() {
            self.status_line = "no tool activity".to_string();
            return;
        }

        self.selected_tool = Some(match self.selected_tool {
            Some(current) => tools
                .iter()
                .copied()
                .find(|index| *index > current)
                .unwrap_or(tools[0]),
            None => tools[0],
        });
        self.status_line = "tool selected".to_string();
    }

    fn select_previous_tool(&mut self) {
        let tools = self.tool_group_indices();
        if tools.is_empty() {
            self.status_line = "no tool calls yet".to_string();
            return;
        }

        self.selected_tool = Some(match self.selected_tool {
            Some(current) => tools
                .iter()
                .rev()
                .copied()
                .find(|index| *index < current)
                .unwrap_or_else(|| *tools.last().unwrap()),
            None => *tools.last().unwrap(),
        });
        self.status_line = "tool selected".to_string();
    }

    fn toggle_selected_tool(&mut self) {
        let Some(index) = self.selected_tool else {
            self.status_line = "no tool selected".to_string();
            return;
        };

        if let Some(TranscriptItem::Reasoning(trace)) = self.transcript.get_mut(index) {
            trace.expanded = !trace.expanded;
            let expanded = trace.expanded;
            self.touch_transcript();
            self.status_line = if expanded {
                "reasoning open".to_string()
            } else {
                "reasoning closed".to_string()
            };
            self.persist_session();
            return;
        }

        let Some((start, end)) = self.tool_group_range_containing(index) else {
            self.selected_tool = None;
            self.status_line = "no tool selected".to_string();
            return;
        };

        let coalescible = tool_group_has_coalesced_runs(&self.transcript, start, end);
        if coalescible && !tool_group_is_open(&self.transcript, start, end) {
            if let Some(run) = self.transcript[start..end]
                .iter_mut()
                .find_map(|item| match item {
                    TranscriptItem::Tool(run) => Some(run),
                    _ => None,
                })
            {
                run.group_expanded = true;
            }
            self.touch_transcript();
            self.selected_tool = Some(start);
            self.status_line = "tool group expanded".to_string();
            self.persist_session();
            return;
        }

        let next_expanded = !self.transcript[start..end]
            .iter()
            .any(|item| matches!(item, TranscriptItem::Tool(run) if run.expanded));

        for item in &mut self.transcript[start..end] {
            if let TranscriptItem::Tool(run) = item {
                run.expanded = false;
                if !next_expanded {
                    run.group_expanded = false;
                }
            }
        }

        if next_expanded
            && let Some(run) = self.transcript[start..end]
                .iter_mut()
                .find_map(|item| match item {
                    TranscriptItem::Tool(run) => Some(run),
                    _ => None,
                })
        {
            run.expanded = true;
        }

        self.touch_transcript();
        self.selected_tool = Some(start);
        self.status_line = if next_expanded {
            "tool details open".to_string()
        } else if coalescible {
            "tool group collapsed".to_string()
        } else {
            "tool details closed".to_string()
        };
        self.persist_session();
    }

    fn attach_or_push_background_tool_start(&mut self, id: &str, command: &str) {
        let summary = format!("$ {command}");
        if let Some(run) = self
            .transcript
            .iter_mut()
            .rev()
            .find_map(|item| match item {
                TranscriptItem::Tool(run)
                    if run.id.is_none()
                        && run.name == "terminal.exec"
                        && run.summary == summary
                        && run.state == ToolRunState::Running =>
                {
                    Some(run)
                }
                _ => None,
            })
        {
            run.id = Some(id.to_string());
            self.touch_transcript();
        } else {
            self.push_tool_start_with_id(
                Some(id.to_string()),
                "terminal.exec".to_string(),
                summary,
            );
        }
    }

    fn update_tool_result_by_id(&mut self, id: &str, state: ToolRunState, detail: &str) {
        let detail = compact_tool_detail(detail);
        if let Some(run) = self
            .transcript
            .iter_mut()
            .rev()
            .find_map(|item| match item {
                TranscriptItem::Tool(run) if run.id.as_deref() == Some(id) => Some(run),
                _ => None,
            })
        {
            queue_or_apply_tool_result(run, state, detail, state == ToolRunState::Failed);
            self.touch_transcript();
        }
        self.persist_session();
    }

    fn close_selected_tool(&mut self) {
        let Some(index) = self.selected_tool else {
            self.status_line = "no tool selected".to_string();
            return;
        };

        if let Some(TranscriptItem::Reasoning(trace)) = self.transcript.get_mut(index) {
            trace.expanded = false;
            self.touch_transcript();
        }

        let mut changed = false;
        if let Some((start, end)) = self.tool_group_range_containing(index) {
            for item in &mut self.transcript[start..end] {
                if let TranscriptItem::Tool(run) = item {
                    run.expanded = false;
                    run.group_expanded = false;
                    changed = true;
                }
            }
        }

        if changed {
            self.touch_transcript();
        }
        self.selected_tool = None;
        self.status_line = "tool closed".to_string();
        self.persist_session();
    }

    fn current_plan(&self) -> Option<&PlanView> {
        self.transcript.iter().rev().find_map(|item| match item {
            TranscriptItem::Plan(plan) => Some(plan),
            _ => None,
        })
    }

    fn apply_plan_update_output(&mut self, output: &str) -> std::result::Result<(), String> {
        let mut plan = serde_json::from_str::<PlanView>(output)
            .map_err(|error| format!("could not parse plan.update output: {error}"))?;
        plan.expanded = false;

        if let Some(TranscriptItem::Plan(existing)) = self.transcript.last_mut() {
            *existing = plan;
        } else {
            self.transcript.push(TranscriptItem::Plan(plan));
        }

        self.touch_transcript();
        self.persist_session();
        Ok(())
    }

    #[cfg(test)]
    fn current_decision(&self) -> Option<&DecisionView> {
        self.transcript.iter().rev().find_map(|item| match item {
            TranscriptItem::Decision(decision) => Some(decision),
            _ => None,
        })
    }

    fn pending_decision(&self) -> Option<&DecisionView> {
        self.transcript.iter().rev().find_map(|item| match item {
            TranscriptItem::Decision(decision) if !decision.answered => Some(decision),
            _ => None,
        })
    }

    fn pending_decision_index(&self) -> Option<usize> {
        self.transcript.iter().rposition(
            |item| matches!(item, TranscriptItem::Decision(decision) if !decision.answered),
        )
    }

    fn apply_decision_request_output(&mut self, output: &str) -> std::result::Result<(), String> {
        let mut decision = serde_json::from_str::<DecisionView>(output)
            .map_err(|error| format!("could not parse decision.request output: {error}"))?;
        decision.answered = false;
        decision.answer = None;
        decision.answers.clear();
        decision.expanded = false;
        self.decision_selection = 0;

        if let Some(TranscriptItem::Decision(existing)) = self.transcript.last_mut()
            && !existing.answered
        {
            *existing = decision;
        } else {
            self.transcript.push(TranscriptItem::Decision(decision));
        }

        self.touch_transcript();
        self.persist_session();
        Ok(())
    }

    fn selected_decision_question_index(&self) -> usize {
        self.pending_decision()
            .map(|decision| {
                self.decision_selection
                    .min(decision.questions.len().saturating_sub(1))
            })
            .unwrap_or(0)
    }

    fn handle_decision_key(&mut self, key: KeyEvent) -> bool {
        if self.pending_decision().is_none()
            || self.slash_suggestions_active()
            || self.mention_popup_visible()
        {
            return false;
        }

        match key.code {
            KeyCode::Down if self.input.is_empty() => {
                self.move_decision_selection(1);
                true
            }
            KeyCode::Up if self.input.is_empty() => {
                self.move_decision_selection(-1);
                true
            }
            KeyCode::Char('j') if self.input.is_empty() && self.selected_decision_is_choice() => {
                self.move_decision_selection(1);
                true
            }
            KeyCode::Char('k') if self.input.is_empty() && self.selected_decision_is_choice() => {
                self.move_decision_selection(-1);
                true
            }
            KeyCode::Char('h') | KeyCode::Left | KeyCode::BackTab
                if self.input.is_empty() && self.selected_decision_is_choice() =>
            {
                self.cycle_selected_decision_choice(-1);
                true
            }
            KeyCode::Char('l') | KeyCode::Right | KeyCode::Tab
                if self.input.is_empty() && self.selected_decision_is_choice() =>
            {
                self.cycle_selected_decision_choice(1);
                true
            }
            KeyCode::Char(ch)
                if self.input.is_empty()
                    && ch.is_ascii_digit()
                    && self.selected_decision_is_choice() =>
            {
                let Some(digit) = ch.to_digit(10) else {
                    return false;
                };
                if digit == 0 {
                    return false;
                }
                self.select_decision_option((digit as usize).saturating_sub(1));
                true
            }
            KeyCode::Char('j' | 'm') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.accept_decision_enter();
                true
            }
            KeyCode::Enter | KeyCode::Char('\n') | KeyCode::Char('\r') => {
                self.accept_decision_enter();
                true
            }
            _ => false,
        }
    }

    fn selected_decision_is_choice(&self) -> bool {
        self.pending_decision()
            .and_then(|decision| {
                decision
                    .questions
                    .get(self.selected_decision_question_index())
            })
            .is_some_and(|question| question.kind == DecisionQuestionKind::Choice)
    }

    fn move_decision_selection(&mut self, amount: isize) {
        let Some(decision) = self.pending_decision() else {
            return;
        };
        let count = decision.questions.len();
        if count == 0 {
            self.decision_selection = 0;
            return;
        }
        self.decision_selection = (self.selected_decision_question_index() as isize + amount)
            .rem_euclid(count as isize) as usize;
        self.status_line = format!(
            "decision question {}/{}",
            self.decision_selection + 1,
            count
        );
    }

    fn cycle_selected_decision_choice(&mut self, amount: isize) {
        let Some((question_id, options, current)) = self.selected_decision_choice_state() else {
            self.status_line = "selected decision expects text".to_string();
            self.toast("Type an answer, then press enter", ToastKind::Info);
            return;
        };
        if options.is_empty() {
            self.status_line = "decision has no options".to_string();
            return;
        }
        let next = (current as isize + amount).rem_euclid(options.len() as isize) as usize;
        let value = options[next].clone();
        self.set_decision_answer(&question_id, value.clone());
        self.status_line = format!("decision option: {}", truncate(&value, 48));
    }

    fn select_decision_option(&mut self, index: usize) {
        let Some((question_id, options, _)) = self.selected_decision_choice_state() else {
            self.status_line = "selected decision expects text".to_string();
            return;
        };
        let Some(value) = options.get(index).cloned() else {
            self.status_line = "option number unavailable".to_string();
            return;
        };
        self.set_decision_answer(&question_id, value.clone());
        self.status_line = format!("decision option: {}", truncate(&value, 48));
    }

    fn selected_decision_choice_state(&self) -> Option<(String, Vec<String>, usize)> {
        let decision = self.pending_decision()?;
        let question = decision
            .questions
            .get(self.selected_decision_question_index())?;
        if question.kind != DecisionQuestionKind::Choice {
            return None;
        }
        let options = question.options.clone();
        if options.is_empty() {
            return Some((question.id.clone(), options, 0));
        }
        let selected = decision
            .answers
            .get(&question.id)
            .and_then(|answer| options.iter().position(|option| option == answer))
            .or_else(|| {
                question
                    .recommended
                    .as_ref()
                    .and_then(|recommended| options.iter().position(|option| option == recommended))
            })
            .unwrap_or(0);
        Some((question.id.clone(), options, selected))
    }

    fn set_decision_answer(&mut self, question_id: &str, value: String) {
        let Some(index) = self.pending_decision_index() else {
            return;
        };
        if let Some(TranscriptItem::Decision(decision)) = self.transcript.get_mut(index) {
            decision
                .answers
                .insert(question_id.to_string(), value.chars().take(600).collect());
            self.touch_transcript();
            self.persist_session();
        }
    }

    fn accept_decision_enter(&mut self) {
        if !self.input.trim().is_empty() {
            self.record_typed_decision_answer();
        } else {
            self.accept_empty_decision_enter();
        }

        if self.pending_decision().is_some_and(decision_ready) {
            self.submit_decision_answer();
        }
    }

    fn record_typed_decision_answer(&mut self) {
        let Some((question_id, question_kind, options)) =
            self.pending_decision().and_then(|decision| {
                decision
                    .questions
                    .get(self.selected_decision_question_index())
                    .map(|question| (question.id.clone(), question.kind, question.options.clone()))
            })
        else {
            return;
        };
        let value = self.input.trim().to_string();
        if value.is_empty() {
            return;
        }

        if question_kind == DecisionQuestionKind::Choice {
            let Some(option) = match_choice_option(&options, &value) else {
                self.status_line = "choose an option with h/l or 1-8".to_string();
                self.toast(
                    "Choice question needs one of the listed options",
                    ToastKind::Warning,
                );
                return;
            };
            self.set_decision_answer(&question_id, option);
        } else {
            self.set_decision_answer(&question_id, value);
        }

        self.input.clear();
        self.input_cursor = 0;
        self.move_to_next_unanswered_decision();
    }

    fn accept_empty_decision_enter(&mut self) {
        let Some((question_id, question_kind, options, recommended)) =
            self.pending_decision().and_then(|decision| {
                decision
                    .questions
                    .get(self.selected_decision_question_index())
                    .map(|question| {
                        (
                            question.id.clone(),
                            question.kind,
                            question.options.clone(),
                            question.recommended.clone(),
                        )
                    })
            })
        else {
            return;
        };

        if self
            .pending_decision()
            .is_some_and(|decision| decision.answers.contains_key(&question_id))
        {
            self.move_to_next_unanswered_decision();
            return;
        }

        if question_kind == DecisionQuestionKind::Choice {
            let choice = recommended
                .filter(|value| options.iter().any(|option| option == value))
                .or_else(|| options.first().cloned());
            if let Some(choice) = choice {
                self.set_decision_answer(&question_id, choice);
                self.move_to_next_unanswered_decision();
            }
        } else {
            self.status_line = "type an answer for this decision".to_string();
            self.toast("Type an answer, then press enter", ToastKind::Info);
        }
    }

    fn move_to_next_unanswered_decision(&mut self) {
        let Some(decision) = self.pending_decision() else {
            return;
        };
        let count = decision.questions.len();
        if count == 0 {
            return;
        }
        for offset in 1..=count {
            let index = (self.selected_decision_question_index() + offset) % count;
            let question = &decision.questions[index];
            if question.required && !decision_question_answered(decision, question) {
                self.decision_selection = index;
                self.status_line = format!("decision question {}/{}", index + 1, count);
                return;
            }
        }
        self.status_line = "decision ready · press enter to send".to_string();
    }

    fn submit_decision_answer(&mut self) {
        if self.is_working() || self.has_active_workflows() {
            self.status_line = "finish current work before answering decision".to_string();
            return;
        }

        let Some(index) = self.pending_decision_index() else {
            return;
        };
        let answer = {
            let Some(TranscriptItem::Decision(decision)) = self.transcript.get(index) else {
                return;
            };
            decision_answer_text(decision)
        };

        if let Some(TranscriptItem::Decision(decision)) = self.transcript.get_mut(index) {
            decision.answered = true;
            decision.answer = Some(answer.clone());
        }
        self.touch_transcript();
        self.transcript
            .push(TranscriptItem::Message(ChatMessage::user(answer.clone())));
        self.touch_transcript();
        self.persist_session();
        self.scroll_chat_to_bottom();
        self.toast("Decision answer sent", ToastKind::Success);
        self.status_line = "decision answer sent".to_string();
        self.start_model_turn(&answer);
    }

    fn tool_group_indices(&self) -> Vec<usize> {
        let mut groups = Vec::new();
        let mut index = 0;
        while index < self.transcript.len() {
            match &self.transcript[index] {
                TranscriptItem::Message(_)
                | TranscriptItem::Workflow(_)
                | TranscriptItem::Plan(_)
                | TranscriptItem::Decision(_) => index += 1,
                TranscriptItem::Tool(_) | TranscriptItem::Reasoning(_) => {
                    let mut first_tool = None;
                    while index < self.transcript.len()
                        && matches!(
                            self.transcript[index],
                            TranscriptItem::Tool(_) | TranscriptItem::Reasoning(_)
                        )
                    {
                        if first_tool.is_none()
                            && matches!(self.transcript[index], TranscriptItem::Tool(_))
                        {
                            first_tool = Some(index);
                        }
                        index += 1;
                    }
                    if let Some(tool_index) = first_tool {
                        groups.push(tool_index);
                    }
                }
            }
        }
        groups
    }

    fn tool_group_range_containing(&self, index: usize) -> Option<(usize, usize)> {
        if !matches!(self.transcript.get(index), Some(TranscriptItem::Tool(_))) {
            return None;
        }

        let mut start = index;
        while start > 0
            && matches!(
                self.transcript[start - 1],
                TranscriptItem::Tool(_) | TranscriptItem::Reasoning(_)
            )
        {
            start -= 1;
        }

        let mut end = index + 1;
        while end < self.transcript.len()
            && matches!(
                self.transcript[end],
                TranscriptItem::Tool(_) | TranscriptItem::Reasoning(_)
            )
        {
            end += 1;
        }

        Some((start, end))
    }

    fn submit_input(&mut self) {
        let task = self.input.trim().to_string();
        if task.is_empty() && self.pending_attachments.is_empty() {
            if self.pending_decision().is_some() {
                self.accept_decision_enter();
            } else if !self.is_working()
                && !self.has_active_workflows()
                && !self.queued_turns.is_empty()
            {
                // Prompts kept from a cancelled turn ([21]) run on an explicit
                // empty submit — never silently auto-launched.
                self.start_next_queued_turn();
            } else {
                self.status_line = "Type a task first.".to_string();
            }
            return;
        }

        // `# <note>` is quick memory: record it in AGENTS.md instead of
        // sending a model turn (works even mid-turn or with a decision
        // pending — a note is never an answer).
        if let Some(note) = task.strip_prefix("# ") {
            let note = note.trim().to_string();
            self.record_quick_memory(&note);
            return;
        }

        if self.pending_decision().is_some()
            && self.pending_attachments.is_empty()
            && !task.starts_with('/')
        {
            self.accept_decision_enter();
            return;
        }

        let attachments = std::mem::take(&mut self.pending_attachments);
        self.attachment_previews.clear();
        self.input.clear();
        self.input_cursor = 0;
        self.refresh_mention_state();
        if attachments.is_empty() && self.run_local_tool_command(&task) {
            self.persist_session();
            self.scroll_chat_to_bottom();
            return;
        }

        if self.is_working() || self.has_active_workflows() {
            if !attachments.is_empty() {
                self.pending_attachments = attachments;
                for attachment in self.pending_attachments.clone() {
                    self.cache_attachment_preview(&attachment);
                }
                self.status_line = "finish current turn before sending images".to_string();
                self.toast("Image turns cannot be queued yet", ToastKind::Warning);
                return;
            }
            self.queued_turns.push_back(task.clone());
            self.status_line = format!(
                "queued: {}{}",
                truncate(&task, 48),
                queue_count_suffix(self.queued_turns.len())
            );
            return;
        }

        self.transcript
            .push(TranscriptItem::Message(ChatMessage::user_with_attachments(
                task.clone(),
                attachments,
            )));
        self.touch_transcript();
        self.persist_session();
        self.scroll_chat_to_bottom();
        self.start_model_turn(&task);
    }

    /// Quick memory: append the note under `## Notes` in AGENTS.md and leave
    /// a muted transcript line. Nothing is sent to the model now — project
    /// instructions are reloaded from AGENTS.md at the start of every turn,
    /// so the note applies from the next turn automatically.
    fn record_quick_memory(&mut self, note: &str) {
        self.input.clear();
        self.input_cursor = 0;
        if note.is_empty() {
            self.status_line = "empty note".to_string();
            self.toast("Nothing to note", ToastKind::Warning);
            return;
        }

        match append_quick_memory(self.tools.workspace(), note) {
            Ok(()) => {
                self.transcript
                    .push(TranscriptItem::Message(ChatMessage::system(format!(
                        "noted in AGENTS.md: {note} (applies from next turn)"
                    ))));
                self.touch_transcript();
                self.persist_session();
                self.scroll_chat_to_bottom();
                self.status_line = "noted in AGENTS.md".to_string();
                self.toast("noted in AGENTS.md", ToastKind::Success);
            }
            Err(error) => {
                self.status_line = format!("note failed: {error}");
                self.toast("Note failed", ToastKind::Error);
            }
        }
    }

    fn run_local_tool_command(&mut self, task: &str) -> bool {
        if task == "/help" || task == "/commands" {
            self.active_modal = Some(if task == "/help" {
                Modal::Help
            } else {
                Modal::Commands
            });
            self.status_line = if task == "/help" {
                "help opened".to_string()
            } else {
                "commands opened".to_string()
            };
            return true;
        }

        if task == "/jobs" {
            self.active_modal = Some(Modal::Jobs);
            self.status_line = format!("{} background jobs", self.background_jobs.len());
            return true;
        }

        if let Some(id) = task.strip_prefix("/kill ") {
            self.kill_background_job(id.trim());
            return true;
        }

        if let Some(id) = task.strip_prefix("/tail ") {
            self.tail_background_job(id.trim());
            return true;
        }

        if let Some(id) = task.strip_prefix("/restart ") {
            self.restart_background_job(id.trim());
            return true;
        }

        if task == "/plan" {
            self.toggle_plan_mode();
            return true;
        }

        if task == "/reload" {
            self.request_reload();
            return true;
        }

        if task == "/workflows" {
            self.active_modal = Some(Modal::Workflows);
            self.status_line = "workflows opened".to_string();
            return true;
        }

        if task == "/workflow" {
            let scripts = WorkflowScript::list(self.tools.workspace());
            let script_lines = if scripts.is_empty() {
                "No saved scripts yet. Add JavaScript workflows under .medusa/workflows/<name>.js"
                    .to_string()
            } else {
                format!(
                    "Saved scripts:\n{}",
                    scripts
                        .iter()
                        .map(|name| format!("  {name}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                )
            };
            self.transcript
                .push(TranscriptItem::Message(ChatMessage::system(format!(
                    "usage: /workflow <script-name> [args]  — run a saved JS workflow script\n       /workflow <task>               — run the built-in recon/implement/verify pipeline\n\n{script_lines}",
                ))));
            self.touch_transcript();
            self.status_line = "workflow needs a task or script".to_string();
            self.toast("Workflow task required", ToastKind::Warning);
            return true;
        }

        if let Some(workflow_task) = task.strip_prefix("/workflow ") {
            let workflow_task = workflow_task.trim();
            let (first, rest) = match workflow_task.split_once(char::is_whitespace) {
                Some((first, rest)) => (first, rest.trim()),
                None => (workflow_task, ""),
            };
            if WorkflowScript::list(self.tools.workspace()).contains(&first.to_string()) {
                self.start_workflow_script(first, rest);
            } else {
                self.start_workflow(workflow_task);
            }
            return true;
        }

        if task == "/sessions" {
            self.active_modal = Some(Modal::Sessions);
            self.status_line = "sessions opened".to_string();
            return true;
        }

        if task == "/tree" {
            self.active_modal = Some(Modal::SessionTree);
            self.status_line = "session tree opened".to_string();
            return true;
        }

        if task == "/resume" {
            self.transcript
                .push(TranscriptItem::Message(ChatMessage::system(
                    "usage: /resume <session>\n\nUse /sessions to inspect saved session names.",
                )));
            self.touch_transcript();
            self.status_line = "resume needs a session".to_string();
            return true;
        }

        if let Some(session_id) = task.strip_prefix("/resume ") {
            self.resume_session(session_id.trim());
            return true;
        }

        if task == "/fork" || task == "/branch" {
            self.fork_session();
            return true;
        }

        if task == "/rewind" {
            self.open_rewind_modal();
            return true;
        }

        if task == "/edit" {
            self.open_edit_message_modal();
            return true;
        }

        if task == "/review" {
            self.run_review_command();
            return true;
        }

        if task == "/cost" {
            self.active_modal = Some(Modal::Cost);
            self.status_line = "token usage opened".to_string();
            return true;
        }

        if task == "/context" {
            self.context_report = Some(self.build_context_report());
            self.active_modal = Some(Modal::Context);
            self.status_line = "context breakdown opened".to_string();
            return true;
        }

        if task == "/compact" {
            self.run_compact_command();
            return true;
        }

        if task == "/clear" {
            self.transcript.clear();
            self.touch_transcript();
            self.selected_tool = None;
            self.context_engine.reset();
            self.toast("Session cleared", ToastKind::Warning);
            self.status_line = "cleared".to_string();
            return true;
        }

        if task == "/settings" {
            self.open_settings_modal();
            return true;
        }

        if task == "/model" {
            self.open_models_modal();
            return true;
        }

        if let Some(model) = task.strip_prefix("/model ") {
            self.set_model_name(model);
            return true;
        }

        if task == "/reasoning" || task == "/effort" || task == "/think" {
            self.open_reasoning_modal();
            return true;
        }

        if let Some(effort) = task
            .strip_prefix("/reasoning ")
            .or_else(|| task.strip_prefix("/effort "))
            .or_else(|| task.strip_prefix("/think "))
        {
            self.set_reasoning_effort(effort);
            return true;
        }

        if task == "/permissions" || task == "/permission" {
            self.open_permissions_modal();
            return true;
        }

        if let Some(mode) = task
            .strip_prefix("/permissions ")
            .or_else(|| task.strip_prefix("/permission "))
        {
            if let Some(mode) = PermissionMode::from_name(mode) {
                self.set_permission_mode(mode);
            } else {
                let available = PermissionMode::all()
                    .iter()
                    .map(|mode| mode.name())
                    .collect::<Vec<_>>()
                    .join(", ");
                self.transcript
                    .push(TranscriptItem::Message(ChatMessage::system(format!(
                        "unknown permission mode: {mode}\n\nAvailable modes: {available}"
                    ))));
                self.touch_transcript();
                self.status_line = "unknown permission mode".to_string();
                self.toast("Unknown permission mode", ToastKind::Error);
            }
            return true;
        }

        if task == "/theme" {
            self.open_themes_modal();
            return true;
        }

        if let Some(theme_name) = task.strip_prefix("/theme ") {
            let theme_name = theme_name.trim();
            if matches!(theme_name, "next" | "+") {
                self.cycle_theme(1);
            } else if matches!(theme_name, "prev" | "previous" | "-") {
                self.cycle_theme(-1);
            } else if let Some(theme) = ThemeKind::from_name(theme_name) {
                self.set_theme(theme);
            } else {
                let available = ThemeKind::all()
                    .iter()
                    .map(|theme| theme.name())
                    .collect::<Vec<_>>()
                    .join(", ");
                self.transcript
                    .push(TranscriptItem::Message(ChatMessage::system(format!(
                        "unknown theme: {theme_name}\n\nAvailable themes: {available}"
                    ))));
                self.touch_transcript();
                self.status_line = "unknown theme".to_string();
                self.toast("Unknown theme", ToastKind::Error);
            }
            return true;
        }

        if task == "/tools" {
            self.transcript
                .push(TranscriptItem::Message(ChatMessage::system(tools_text())));
            self.touch_transcript();
            self.status_line = "tool surface listed".to_string();
            self.toast("Tool surface listed", ToastKind::Info);
            return true;
        }

        if task == "/skills" {
            self.transcript
                .push(TranscriptItem::Message(ChatMessage::system(
                    self.tools.skills().list_text(),
                )));
            self.touch_transcript();
            self.status_line = "skills listed".to_string();
            self.toast("Workspace skills listed", ToastKind::Info);
            return true;
        }

        if task == "/agents" {
            self.agent_registry = AgentRegistry::load(self.tools.workspace()).unwrap_or_default();
            self.active_modal = Some(Modal::Agents);
            self.status_line = "named agents".to_string();
            return true;
        }

        if task == "/mcp" || task.starts_with("/mcp ") {
            let args = task.strip_prefix("/mcp").unwrap_or_default().trim();
            if args.is_empty() {
                self.mcp_statuses = self.mcp.statuses();
                self.active_modal = Some(Modal::Mcp);
                self.status_line = "mcp servers".to_string();
                return true;
            }
            let Some(name) = args.strip_prefix("restart ").map(str::trim) else {
                self.status_line = "usage: /mcp [restart <server>]".to_string();
                self.toast("Usage: /mcp [restart <server>]", ToastKind::Warning);
                return true;
            };
            if !self.mcp.has_server(name) {
                self.status_line = "unknown mcp server".to_string();
                self.toast(format!("Unknown MCP server: {name}"), ToastKind::Error);
                return true;
            }
            // Restarting spawns and handshakes (seconds); never on the UI
            // thread. Progress is visible by reopening /mcp.
            let registry = self.mcp.clone();
            let server = name.to_string();
            self.status_line = format!("restarting mcp server {name}");
            self.toast(format!("Restarting MCP server {name}"), ToastKind::Info);
            thread::spawn(move || {
                let _ = registry.restart(&server);
            });
            return true;
        }

        if task == "/auth" {
            self.transcript
                .push(TranscriptItem::Message(ChatMessage::system(
                    probe_codex_auth().summary_lines().join("\n"),
                )));
            self.touch_transcript();
            self.status_line = "auth probed".to_string();
            self.toast("Auth status checked", ToastKind::Success);
            return true;
        }

        if let Some(command) = task.strip_prefix("/exec ") {
            let (command, background) = parse_exec_command(command);
            self.start_exec_command(command, background);
            return true;
        }

        if let Some(path) = task.strip_prefix("/patch ") {
            self.push_tool_start("file.patch".to_string(), path.to_string());
            self.transcript.push(TranscriptItem::Message(ChatMessage::system(format!(
                "permission check · file.patch\nsource: {path}\nrisk: mutation\npreview: reading diff before apply"
            ))));
            self.touch_transcript();
            let result = match self.tools.read_patch_file(path) {
                Ok(diff) => {
                    let preview = diff.lines().take(24).collect::<Vec<_>>().join("\n");
                    self.transcript
                        .push(TranscriptItem::Message(ChatMessage::system(format!(
                            "diff preview · {path}\n{preview}{}",
                            if diff.lines().count() > 24 {
                                "\n…"
                            } else {
                                ""
                            }
                        ))));
                    self.touch_transcript();
                    self.user_tools().file_patch(FilePatchRequest::new(diff))
                }
                Err(error) => Err(error),
            };

            match result {
                Ok(result) => {
                    let files = result.changed_files.join(", ");
                    self.push_tool_result("file.patch", format!("patched files:\n{files}"));
                    self.status_line = "file.patch applied".to_string();
                    self.toast("Patch applied", ToastKind::Success);
                }
                Err(error) => {
                    self.push_tool_result("file.patch", format!("error: {error}"));
                    self.status_line = "file.patch failed".to_string();
                    self.toast("Patch failed", ToastKind::Error);
                }
            }
            return true;
        }

        if task.starts_with('/') {
            let command = task.split_whitespace().next().unwrap_or(task);
            self.transcript
                .push(TranscriptItem::Message(ChatMessage::system(format!(
                    "unknown command: {command}\n\nType /help to see available commands."
                ))));
            self.touch_transcript();
            self.status_line = "unknown command".to_string();
            return true;
        }

        false
    }

    fn set_theme(&mut self, theme: ThemeKind) {
        self.theme = theme;
        self.theme_selection = theme_index(theme);
        set_active_theme(theme);
        self.invalidate_render_cache();
        self.status_line = format!("theme: {}", theme.name());
        match save_theme_preference(self.tools.workspace(), theme) {
            Ok(()) => self.toast(
                format!("Theme set to {}", theme.label()),
                ToastKind::Success,
            ),
            Err(error) => self.toast(format!("Theme set, save failed: {error}"), ToastKind::Error),
        }
    }

    fn cycle_theme(&mut self, offset: isize) {
        let theme = theme_at_offset(self.theme, offset);
        self.set_theme(theme);
    }

    fn set_model_name(&mut self, model: &str) {
        let model = model.trim();
        if model.is_empty() {
            self.toast("Model cannot be empty", ToastKind::Error);
            self.status_line = "model unchanged".to_string();
            return;
        }

        self.model.set_model_name(model.to_string());
        self.model_selection = model_index(model);
        self.status_line = format!("model: {model}");
        match save_model_preference(self.tools.workspace(), model) {
            Ok(()) => self.toast(format!("Model set to {model}"), ToastKind::Success),
            Err(error) => self.toast(format!("Model set, save failed: {error}"), ToastKind::Error),
        }
    }

    fn set_reasoning_effort(&mut self, effort: &str) {
        let effort = effort.trim();
        if effort.is_empty() {
            self.toast("Reasoning effort cannot be empty", ToastKind::Error);
            return;
        }
        self.model.set_reasoning_effort(effort.to_string());
        self.reasoning_selection = reasoning_index(self.model.model_name(), effort);
        self.status_line = format!("reasoning: {effort}");
        match save_reasoning_preference(self.tools.workspace(), effort) {
            Ok(()) => self.toast(
                format!("Reasoning effort set to {effort}"),
                ToastKind::Success,
            ),
            Err(error) => self.toast(
                format!("Effort set, save failed: {error}"),
                ToastKind::Error,
            ),
        }
    }

    fn set_permission_mode(&mut self, mode: PermissionMode) {
        let workspace = self.tools.workspace().to_path_buf();
        self.permission_mode = mode;
        self.permission_selection = permission_mode_index(mode);
        self.status_line = format!("permissions: {}", mode.name());

        match save_permission_mode_preference(&workspace, mode).and_then(|_| {
            ToolRuntime::new(&workspace).map(|runtime| (runtime.with_mcp(self.mcp.clone()), ()))
        }) {
            Ok((runtime, ())) => {
                self.tools = runtime;
                self.toast(
                    format!("Permissions set to {}", mode.label()),
                    ToastKind::Success,
                );
            }
            Err(error) => {
                self.toast(
                    format!("Permissions set, reload failed: {error}"),
                    ToastKind::Error,
                );
            }
        }
    }

    fn permission_status_prefix(&self) -> Option<&'static str> {
        match self.permission_mode {
            PermissionMode::Open => None,
            PermissionMode::Guarded => Some("guarded"),
            PermissionMode::Ask => Some("ask"),
            PermissionMode::Readonly => Some("readonly"),
        }
    }

    fn scoped_status(&self, status: impl AsRef<str>) -> String {
        match self.permission_status_prefix() {
            Some(prefix) => format!("{prefix} · {}", status.as_ref()),
            None => status.as_ref().to_string(),
        }
    }

    fn resume_session(&mut self, session_id: &str) {
        if self.model_events.is_some() {
            self.status_line = "finish current turn before resuming".to_string();
            self.toast("Cannot resume while working", ToastKind::Warning);
            return;
        }

        if session_id.is_empty() {
            self.status_line = "resume needs a session".to_string();
            self.toast("Session name required", ToastKind::Warning);
            return;
        }

        self.persist_session();

        let Some(session) = self.session.as_mut() else {
            self.status_line = "session storage disabled".to_string();
            self.toast("No session storage", ToastKind::Warning);
            return;
        };

        match session.switch_to(session_id) {
            Ok(transcript) => {
                let name = session.current_id();
                self.transcript = transcript;
                self.context_engine.reset();
                self.touch_transcript();
                self.selected_tool = None;
                self.streaming_message = None;
                self.scroll_chat_to_bottom();
                self.status_line = format!("resumed {name}");
                self.toast("Session resumed", ToastKind::Success);
            }
            Err(error) => {
                self.status_line = format!("resume failed: {error}");
                self.toast("Resume failed", ToastKind::Error);
            }
        }
    }

    fn fork_session(&mut self) {
        if self.model_events.is_some() {
            self.status_line = "finish current turn before forking".to_string();
            self.toast("Cannot fork while working", ToastKind::Warning);
            return;
        }

        let Some(session) = self.session.as_mut() else {
            self.status_line = "session storage disabled".to_string();
            self.toast("No session to fork", ToastKind::Warning);
            return;
        };

        match session.fork(&self.transcript) {
            Ok(name) => {
                self.selected_tool = None;
                self.status_line = format!("forked {name}");
                self.toast("Session forked", ToastKind::Success);
            }
            Err(error) => {
                self.status_line = format!("fork failed: {error}");
                self.toast("Fork failed", ToastKind::Error);
            }
        }
    }

    fn has_running_background_jobs(&self) -> bool {
        self.background_jobs
            .values()
            .any(|job| job.state == ToolRunState::Running)
    }

    /// A rewind touches the same files a running turn or background job may
    /// be writing; refuse instead of racing them.
    fn rewind_blocked_reason(&self) -> Option<&'static str> {
        if self.is_working() || self.has_active_workflows() {
            Some("finish the current turn before rewinding")
        } else if self.has_running_background_jobs() {
            Some("stop background jobs before rewinding")
        } else {
            None
        }
    }

    fn open_rewind_modal(&mut self) {
        if let Some(reason) = self.rewind_blocked_reason() {
            self.status_line = reason.to_string();
            self.toast("Cannot rewind now", ToastKind::Warning);
            return;
        }

        let entries = CheckpointStore::open(self.tools.workspace())
            .and_then(|store| store.list())
            .unwrap_or_default();
        if entries.is_empty() {
            self.status_line = "no checkpoints yet".to_string();
            self.toast("No checkpoints to rewind to", ToastKind::Info);
            return;
        }

        self.rewind_entries = entries;
        self.rewind_selection = 0;
        self.rewind_stage = RewindStage::Pick;
        self.rewind_confirm_selection = 0;
        self.active_modal = Some(Modal::Rewind);
        self.status_line = "rewind opened".to_string();
    }

    fn selected_rewind_entry(&self) -> Option<&CheckpointEntry> {
        self.rewind_entries.get(self.rewind_selection)
    }

    /// Fork is only offered for checkpoints from the current session; grafting
    /// a foreign transcript onto this session would be nonsense.
    ///
    /// Session-id equality is necessary but NOT sufficient: `/clear` empties
    /// the transcript without rotating the session id, so a pre-clear
    /// checkpoint's `transcript_user_index` no longer maps to its user message.
    /// Forking on that stale index truncates the live transcript at a
    /// meaningless row. So also require the recorded index to still point at a
    /// live user message whose prompt matches the checkpoint's excerpt; if it
    /// does not, only file-only restore is offered.
    fn selected_rewind_offers_fork(&self) -> bool {
        match (self.selected_rewind_entry(), self.session.as_ref()) {
            (Some(entry), Some(session)) => {
                entry.session_id == session.current_id() && self.checkpoint_index_is_live(entry)
            }
            _ => false,
        }
    }

    /// True when the checkpoint's recorded user-message row still exists in the
    /// current transcript, is a user message, and its prompt still matches the
    /// checkpoint's excerpt — i.e. forking at that index would land on the
    /// prompt the checkpoint was taken for, not a post-`/clear` coincidence.
    fn checkpoint_index_is_live(&self, entry: &CheckpointEntry) -> bool {
        matches!(
            self.transcript.get(entry.transcript_user_index),
            Some(TranscriptItem::Message(message))
                if message.role == ChatRole::User
                    && excerpt_for_checkpoint(&message.content) == entry.prompt_excerpt
        )
    }

    /// Confirm-screen options: file-only restore is the default; fork is an
    /// extra option when available; cancel is always last.
    fn rewind_confirm_options(&self) -> Vec<&'static str> {
        if self.selected_rewind_offers_fork() {
            vec![
                "Restore files",
                "Restore files + fork conversation",
                "Cancel",
            ]
        } else {
            vec!["Restore files", "Cancel"]
        }
    }

    fn handle_rewind_key(&mut self, key: KeyEvent) {
        match self.rewind_stage {
            RewindStage::Pick => match key.code {
                KeyCode::Esc => self.close_modal(),
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.should_quit = true;
                }
                KeyCode::Up | KeyCode::BackTab => {
                    self.rewind_selection = self.rewind_selection.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Tab => {
                    self.rewind_selection = (self.rewind_selection + 1)
                        .min(self.rewind_entries.len().saturating_sub(1));
                }
                KeyCode::Home => self.rewind_selection = 0,
                KeyCode::End => {
                    self.rewind_selection = self.rewind_entries.len().saturating_sub(1);
                }
                KeyCode::Enter => {
                    if self.selected_rewind_entry().is_some() {
                        self.rewind_stage = RewindStage::Confirm;
                        self.rewind_confirm_selection = 0;
                    }
                }
                _ => {}
            },
            RewindStage::Confirm => match key.code {
                KeyCode::Esc => {
                    self.rewind_stage = RewindStage::Pick;
                }
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.should_quit = true;
                }
                KeyCode::Up | KeyCode::BackTab => {
                    self.rewind_confirm_selection = self.rewind_confirm_selection.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Tab => {
                    self.rewind_confirm_selection = (self.rewind_confirm_selection + 1)
                        .min(self.rewind_confirm_options().len().saturating_sub(1));
                }
                KeyCode::Enter => self.accept_rewind_confirm(),
                _ => {}
            },
        }
    }

    fn accept_rewind_confirm(&mut self) {
        let options = self.rewind_confirm_options();
        let choice = options
            .get(self.rewind_confirm_selection)
            .copied()
            .unwrap_or("Cancel");
        match choice {
            "Restore files" => self.execute_rewind_restore(false),
            "Restore files + fork conversation" => self.execute_rewind_restore(true),
            _ => {
                self.rewind_stage = RewindStage::Pick;
            }
        }
    }

    fn execute_rewind_restore(&mut self, fork: bool) {
        if let Some(reason) = self.rewind_blocked_reason() {
            self.status_line = reason.to_string();
            self.toast("Cannot rewind now", ToastKind::Warning);
            return;
        }
        let Some(entry) = self.selected_rewind_entry().cloned() else {
            self.close_modal();
            return;
        };

        let report = match CheckpointStore::open(self.tools.workspace())
            .and_then(|store| store.restore(&entry.id))
        {
            Ok(report) => report,
            Err(error) => {
                self.close_modal();
                self.status_line = format!("rewind failed: {error}");
                self.toast(format!("Rewind failed: {error}"), ToastKind::Error);
                return;
            }
        };

        self.close_modal();
        let rewound = report.restored.len() + report.deleted.len();
        let mut message = format!(
            "Rewound {rewound} file{}",
            if rewound == 1 { "" } else { "s" }
        );
        if !report.skipped.is_empty() {
            message.push_str(&format!(
                " · {} not rewindable (too large or symlink)",
                report.skipped.len()
            ));
        }
        if !report.refused.is_empty() {
            message.push_str(&format!(
                " · {} refused (would escape workspace)",
                report.refused.len()
            ));
        }
        let toast_kind = if report.refused.is_empty() {
            ToastKind::Success
        } else {
            ToastKind::Error
        };
        self.toast(message, toast_kind);
        self.status_line = format!("rewound to before {}", truncate(&entry.prompt_excerpt, 48));

        if fork {
            self.fork_transcript_at_checkpoint(&entry);
        }
    }

    /// Fork the conversation back to the checkpoint's turn: drop that user
    /// message and everything after it, fork the session file, and put the
    /// old prompt back in the composer for editing.
    ///
    /// Refuses (leaving files-only restore intact) when the checkpoint's
    /// recorded index no longer maps to its user message — e.g. after `/clear`
    /// invalidated all indices without rotating the session id. Forking on a
    /// stale index would truncate the live transcript at a meaningless row.
    fn fork_transcript_at_checkpoint(&mut self, entry: &CheckpointEntry) {
        if !self.checkpoint_index_is_live(entry) {
            self.status_line = "fork skipped: checkpoint predates the current conversation".into();
            self.toast(
                "Files restored; conversation left as-is (checkpoint is from a cleared timeline)",
                ToastKind::Warning,
            );
            return;
        }
        if let Some(name) = self.fork_transcript_before(entry.transcript_user_index) {
            self.status_line = format!("forked {name}");
            self.toast("Conversation forked at checkpoint", ToastKind::Success);
        }
    }

    /// Shared backtrack core: truncate the live transcript to just before
    /// the user message at `index`, fork the session file so the original
    /// timeline stays reachable via /tree, and put the dropped prompt back
    /// in the composer for editing. Returns the forked session name.
    fn fork_transcript_before(&mut self, index: usize) -> Option<String> {
        let Some(session) = self.session.as_mut() else {
            self.toast("No session to fork", ToastKind::Warning);
            return None;
        };

        let index = index.min(self.transcript.len());
        let old_prompt = match self.transcript.get(index) {
            Some(TranscriptItem::Message(message)) if message.role == ChatRole::User => {
                message.content.clone()
            }
            _ => String::new(),
        };

        self.transcript.truncate(index);
        match session.fork(&self.transcript) {
            Ok(name) => {
                self.touch_transcript();
                self.selected_tool = None;
                self.streaming_message = None;
                self.context_engine.reset();
                self.scroll_chat_to_bottom();
                self.input = old_prompt;
                self.input_cursor = self.input_len();
                Some(name)
            }
            Err(error) => {
                self.status_line = format!("fork failed: {error}");
                self.toast("Fork failed", ToastKind::Error);
                None
            }
        }
    }

    /// `/edit`: open the backtrack picker over previous user messages.
    /// Double-Esc on an idle composer is already the quit gesture, so
    /// backtracking lives on a slash command instead of a key chord.
    fn open_edit_message_modal(&mut self) {
        if self.is_working() {
            self.status_line = "finish the current turn before editing a message".to_string();
            self.toast("Cannot edit while a turn is running", ToastKind::Warning);
            return;
        }
        let entries = self
            .transcript
            .iter()
            .enumerate()
            .rev()
            .filter_map(|(index, item)| match item {
                TranscriptItem::Message(message) if message.role == ChatRole::User => {
                    Some(EditPickerEntry {
                        transcript_index: index,
                        preview: message_one_liner(&message.content, 60),
                    })
                }
                _ => None,
            })
            .take(EDIT_PICKER_LIMIT)
            .collect::<Vec<_>>();
        if entries.is_empty() {
            self.status_line = "no previous messages to edit".to_string();
            self.toast("Nothing to edit yet", ToastKind::Info);
            return;
        }
        self.edit_picker_entries = entries;
        self.edit_picker_selection = 0;
        self.active_modal = Some(Modal::EditMessage);
        self.status_line = "pick a message to edit".to_string();
    }

    fn handle_edit_message_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.close_modal(),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true;
            }
            KeyCode::Up | KeyCode::BackTab => {
                self.edit_picker_selection = self.edit_picker_selection.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Tab => {
                self.edit_picker_selection = (self.edit_picker_selection + 1)
                    .min(self.edit_picker_entries.len().saturating_sub(1));
            }
            KeyCode::Home => self.edit_picker_selection = 0,
            KeyCode::End => {
                self.edit_picker_selection = self.edit_picker_entries.len().saturating_sub(1);
            }
            KeyCode::Enter => self.accept_edit_message_selection(),
            _ => {}
        }
    }

    fn accept_edit_message_selection(&mut self) {
        let Some(index) = self
            .edit_picker_entries
            .get(self.edit_picker_selection)
            .map(|entry| entry.transcript_index)
        else {
            self.close_modal();
            return;
        };
        self.active_modal = None;
        if self.is_working() {
            self.status_line = "finish the current turn before editing a message".to_string();
            self.toast("Cannot edit while a turn is running", ToastKind::Warning);
            return;
        }
        if self.fork_transcript_before(index).is_some() {
            self.status_line =
                "editing message — enter resends from here (original timeline kept in /tree)"
                    .to_string();
            self.toast(
                "Backtracked — original timeline kept in /tree",
                ToastKind::Success,
            );
        }
    }

    /// `/review`: seed the composer with a code-review prompt (never
    /// auto-sent, so scope can be trimmed first). Toasts instead when the
    /// workspace has no git repo or no pending changes.
    fn run_review_command(&mut self) {
        if !(self.review_diff_check)(self.tools.workspace()) {
            self.status_line = "nothing to review".to_string();
            self.toast("Nothing to review", ToastKind::Info);
            return;
        }
        self.input = REVIEW_PROMPT_TEMPLATE.to_string();
        self.input_cursor = self.input_len();
        self.status_line = "review prompt ready — edit and press enter".to_string();
    }

    fn start_model_turn(&mut self, _task: &str) {
        if !self.model_enabled {
            self.status_line = "queued".to_string();
            return;
        }

        if self.model_events.is_some() || self.streaming_message.is_some() {
            self.queued_turns.push_back(_task.to_string());
            self.status_line = format!(
                "queued: {}{}",
                truncate(_task, 48),
                queue_count_suffix(self.queued_turns.len())
            );
            return;
        }

        let assistant_index = self.transcript.len();
        self.transcript
            .push(TranscriptItem::Message(ChatMessage::assistant("")));
        self.touch_transcript();
        self.persist_session();
        self.streaming_message = Some(assistant_index);
        self.last_stream_save = Instant::now();
        self.status_line = self.scoped_status("streaming");
        self.turn_started_at = Some(Instant::now());

        self.denied_this_turn.clear();
        self.denied_edits_this_turn.clear();
        self.turn_usage = TokenUsage::default();
        self.turn_requests = 0;
        let backend = self.model.clone();
        let permission_mode = self.permission_mode;
        // Per-turn checkpoint recorder: mutating file tools capture pre-images
        // through it, keyed to this turn's user message row.
        let transcript_user_index = self.transcript[..assistant_index]
            .iter()
            .rposition(|item| {
                matches!(
                    item,
                    TranscriptItem::Message(ChatMessage {
                        role: ChatRole::User,
                        ..
                    })
                )
            })
            .unwrap_or(0);
        let recorder = CheckpointRecorder::new(
            self.tools.workspace(),
            CheckpointMeta {
                session_id: self
                    .session
                    .as_ref()
                    .map(SessionStore::current_id)
                    .unwrap_or_default(),
                prompt_excerpt: excerpt_for_checkpoint(_task),
                transcript_user_index,
            },
        );
        self.active_checkpoint = Some(recorder.clone());
        let cancel = CancelToken::new();
        self.turn_cancel = Some(cancel.clone());
        self.cancel_requested_at = None;
        let tools = self
            .tools
            .clone()
            .with_background_events(self.background_job_sender.clone())
            .with_approval_handler(self.approval_handler.clone())
            .with_checkpoint_recorder(recorder)
            .with_cancel_token(cancel.clone());
        #[cfg(test)]
        {
            self.last_turn_runtime = Some(tools.clone());
        }
        let history = self.conversation_history();
        let context_engine = self.context_engine.clone();
        let plan_mode = self.plan_mode;
        let (sender, receiver) = mpsc::channel();
        self.model_events = Some(receiver);

        thread::spawn(move || {
            // Compaction may call the model to summarize old history, so it
            // runs here on the worker thread, never on the UI thread. It
            // shares the turn's cancel token so Esc interrupts it too.
            let prompt = context_engine.prepare(&history, &backend, &cancel);
            let result = if permission_mode == PermissionMode::Readonly || plan_mode {
                backend.chat_stream_messages_read_only(&prompt, tools, |event| {
                    sender.send(event).map_err(|error| {
                        color_eyre::eyre::eyre!("failed to send stream event: {error}")
                    })?;
                    Ok(())
                })
            } else {
                backend.chat_stream_messages(&prompt, tools, |event| {
                    sender.send(event).map_err(|error| {
                        color_eyre::eyre::eyre!("failed to send stream event: {error}")
                    })?;
                    Ok(())
                })
            };

            match result {
                Ok(event_count) => {
                    let _ = sender.send(ModelStreamEvent::Done { event_count });
                }
                Err(error) if error_is_cancellation(&error) => {
                    let _ = sender.send(ModelStreamEvent::Cancelled);
                }
                Err(error) => {
                    let _ = sender.send(ModelStreamEvent::Error(error.to_string()));
                }
            }
        });
    }

    fn is_working(&self) -> bool {
        self.model_events.is_some() || self.streaming_message.is_some()
    }

    /// BEL when a long-running turn needs attention (approval prompt) or
    /// ends (complete/error/cancel); [`should_ring_bell`] holds the gating.
    fn ring_bell_if_due(&self) {
        let enabled = bell_enabled(self.bell_setting, env::var("MEDUSA_BELL").ok().as_deref());
        let working_for = self.turn_started_at.map(|started| started.elapsed());
        if should_ring_bell(enabled, working_for) {
            let mut stdout = io::stdout();
            let _ = stdout.write_all(b"\x07");
            let _ = stdout.flush();
        }
    }

    /// First Esc while working: flip the turn's cancel token and unblock a
    /// worker that may be parked on an approval prompt by denying everything
    /// queued. The worker unwinds cooperatively and reports Cancelled.
    fn request_cancel_turn(&mut self) {
        if let Some(token) = &self.turn_cancel {
            token.cancel();
        }
        while let Some(pending) = self.approval_queue.pop_front() {
            let _ = pending.respond.send(ApprovalDecision::Deny);
        }
        self.approval_shown_at = None;
        self.cancel_requested_at = Some(Instant::now());
        self.status_line = "cancelling… esc again to force-stop".to_string();
    }

    /// Second Esc while cancelling: stop waiting for the worker. Dropping the
    /// receiver makes its next send fail, so the thread dies on its own.
    fn force_abandon_turn(&mut self) {
        self.model_events = None;
        self.finalize_cancelled_turn("turn abandoned");
    }

    /// Close out an interrupted turn: resolve every still-running transcript
    /// row, leave a muted system note (which also re-enters model history so
    /// the conversation resumes coherently), and clear all turn state.
    /// Partial assistant text stays in the transcript.
    /// Freeze the streaming turn's usage into the last-turn readout. Guarded
    /// so a turn that failed before any request keeps the previous readout.
    fn record_turn_usage_totals(&mut self) {
        if self.turn_requests > 0 {
            self.last_turn_usage = self.turn_usage;
            self.last_turn_requests = self.turn_requests;
        }
    }

    fn finalize_cancelled_turn(&mut self, status: &str) {
        self.record_turn_usage_totals();
        self.ring_bell_if_due();
        for item in &mut self.transcript {
            match item {
                TranscriptItem::Tool(run) if run.state == ToolRunState::Running => {
                    apply_tool_result_now(
                        run,
                        ToolRunState::Failed,
                        "cancelled".to_string(),
                        false,
                    );
                }
                TranscriptItem::Workflow(view) => mark_workflow_view_cancelled(view),
                _ => {}
            }
        }
        // The turn's views are marked cancelled above; actually stop the
        // background workflow workers so their file-mutating tools bail (their
        // checkpoints are then finalized when the receiver disconnects).
        for workflow in &self.workflow_events {
            workflow.cancel.cancel();
        }
        self.transcript
            .push(TranscriptItem::Message(ChatMessage::system(
                TURN_INTERRUPTED_NOTE,
            )));
        self.touch_transcript();
        self.stick_chat_to_bottom_if_needed();

        self.streaming_message = None;
        self.turn_started_at = None;
        self.turn_cancel = None;
        self.cancel_requested_at = None;
        self.status_line = status.to_string();
        // Keep the follow-up prompts the user explicitly queued (the UI
        // acknowledged each with "queued: …"). Silently dropping them on cancel
        // loses text the user believes is pending; instead we hold them and say
        // so — an empty submit while idle runs the next one.
        if !self.queued_turns.is_empty() {
            let count = self.queued_turns.len();
            self.toast(
                format!(
                    "{count} queued prompt{} kept — submit an empty line to run",
                    if count == 1 { "" } else { "s" }
                ),
                ToastKind::Info,
            );
        }
        self.persist_session();
        self.finish_turn_checkpoint();
    }

    fn has_active_workflows(&self) -> bool {
        !self.workflow_events.is_empty()
    }

    /// Cancel every background workflow: flip each run's shared cancel token so
    /// its subagents' model streams and file-mutating tools bail cooperatively,
    /// and mark the visible workflow rows cancelled now. The workers are
    /// removed from `workflow_events` once their receivers disconnect
    /// (`drain_workflow_events`), which also finalizes their checkpoints; the JS
    /// orchestration loop may run to its next await before it observes the
    /// token, so the rows show "cancelled" a beat before the thread exits.
    fn cancel_active_workflows(&mut self) {
        if self.workflow_events.is_empty() {
            return;
        }
        let count = self.workflow_events.len();
        for workflow in &self.workflow_events {
            workflow.cancel.cancel();
        }
        for view in &mut self.workflows {
            mark_workflow_view_cancelled(view);
        }
        let mut transcript_changed = false;
        for item in &mut self.transcript {
            if let TranscriptItem::Workflow(view) = item {
                mark_workflow_view_cancelled(view);
                transcript_changed = true;
            }
        }
        if transcript_changed {
            self.touch_transcript();
        }
        self.status_line = "cancelling background workflow…".to_string();
        self.toast(
            format!(
                "Cancelling {count} background workflow{}",
                if count == 1 { "" } else { "s" }
            ),
            ToastKind::Warning,
        );
    }

    /// `/compact`: fold older history into the ContextEngine summary now
    /// instead of waiting for the budget to force it. Summarization calls the
    /// model, so it runs on a worker thread; the result lands via
    /// [`Self::drain_compact_events`].
    fn run_compact_command(&mut self) {
        if self.is_working() {
            self.status_line = "compact unavailable while a turn is running".to_string();
            self.toast("Cannot compact while a turn is running", ToastKind::Warning);
            return;
        }
        if self.compact_events.is_some() {
            self.status_line = "compaction already running".to_string();
            self.toast("Compaction already running", ToastKind::Info);
            return;
        }
        if !self.model_enabled {
            self.status_line = "compact needs the model backend".to_string();
            self.toast("Compaction needs the model backend", ToastKind::Warning);
            return;
        }

        let history = self.conversation_history();
        let engine = self.context_engine.clone();
        let backend = self.model.clone();
        let (sender, receiver) = mpsc::channel();
        self.compact_events = Some(receiver);
        self.status_line = "compacting context…".to_string();

        thread::spawn(move || {
            let cancel = CancelToken::new();
            let result = engine
                .compact_now(&history, &backend, &cancel)
                .map_err(|error| error.to_string());
            let _ = sender.send(result);
        });
    }

    fn drain_compact_events(&mut self) -> bool {
        let Some(receiver) = &self.compact_events else {
            return false;
        };
        let outcome = match receiver.try_recv() {
            Ok(outcome) => outcome,
            Err(TryRecvError::Empty) => return false,
            Err(TryRecvError::Disconnected) => Err("compaction worker exited".to_string()),
        };
        self.compact_events = None;
        match outcome {
            Ok(compaction) => {
                self.status_line = "context compacted".to_string();
                self.toast(
                    format!(
                        "Compacted: est. {} → {} ({} messages folded)",
                        format_token_count(compaction.before_tokens as u64),
                        format_token_count(compaction.after_tokens as u64),
                        compaction.folded_messages
                    ),
                    ToastKind::Success,
                );
            }
            Err(error) => {
                self.status_line = "compact failed".to_string();
                self.toast(format!("Compact failed: {error}"), ToastKind::Error);
            }
        }
        true
    }

    fn drain_pending_tool_results(&mut self) -> bool {
        let mut changed = false;
        for item in &mut self.transcript {
            if let TranscriptItem::Tool(run) = item
                && run.state == ToolRunState::Running
                && run.started_at.elapsed() >= MIN_TOOL_PULSE_VISIBLE
                && let Some(pending) = run.pending_result.take()
            {
                let expand = pending.state == ToolRunState::Failed;
                apply_tool_result_now(run, pending.state, pending.detail, expand);
                changed = true;
            }
        }

        if changed {
            self.touch_transcript();
            self.persist_session();
        }

        changed
    }

    fn has_running_tool_rows(&self) -> bool {
        self.transcript.iter().any(|item| {
            matches!(
                item,
                TranscriptItem::Tool(ToolRun {
                    state: ToolRunState::Running,
                    ..
                })
            )
        })
    }

    fn has_running_workflow_rows(&self) -> bool {
        self.transcript.iter().any(|item| {
            matches!(
                item,
                TranscriptItem::Workflow(WorkflowRunView {
                    status: WorkflowViewState::Running,
                    ..
                })
            )
        })
    }

    fn has_active_animation(&self) -> bool {
        self.is_working()
            || self.has_active_workflows()
            || self.has_running_tool_rows()
            || self.has_running_workflow_rows()
    }

    fn touch_transcript(&mut self) {
        self.transcript_version = self.transcript_version.wrapping_add(1);
        self.transcript_rows_cache = None;
        self.last_transcript_rows = Arc::new(Vec::new());
    }

    fn invalidate_render_cache(&mut self) {
        self.transcript_rows_cache = None;
        self.last_transcript_rows = Arc::new(Vec::new());
        self.attachment_previews.clear();
    }

    fn set_workflow_status_line(&mut self, status: impl Into<String>) {
        if !self.is_working() {
            self.status_line = status.into();
        }
    }

    fn request_reload(&mut self) {
        if self.is_working() || self.has_active_workflows() {
            self.status_line = "reload blocked: work is still running".to_string();
            self.toast("Wait for active work before reloading", ToastKind::Warning);
            return;
        }

        if !self.queued_turns.is_empty() {
            self.status_line = "reload blocked: queued turns would be lost".to_string();
            self.toast("Finish queued turns before reloading", ToastKind::Warning);
            return;
        }

        self.persist_session();
        if let Err(error) = save_theme_preference(self.tools.workspace(), self.theme) {
            self.toast(
                format!("Theme save failed before reload: {error}"),
                ToastKind::Warning,
            );
        }
        // Also pass the active theme through the process environment. This covers reloads
        // from older builds that did not persist theme settings yet, or workspaces where
        // writing .medusa/settings.json failed.
        unsafe { env::set_var("MEDUSA_RELOAD_THEME", self.theme.name()) };
        self.status_line = "reloading Medusa…".to_string();
        self.toast("Reloading Medusa", ToastKind::Info);
        self.restart_requested = true;
        self.should_quit = true;
    }

    fn start_workflow(&mut self, task: &str) {
        if task.trim().is_empty() {
            self.status_line = "workflow needs a task".to_string();
            self.toast("Workflow task required", ToastKind::Warning);
            return;
        }

        if !self.model_enabled {
            self.status_line = "workflow queued".to_string();
            return;
        }

        if self.is_working() {
            self.queued_turns
                .push_back(format!("/workflow {}", task.trim()));
            self.status_line = format!(
                "queued workflow: {}{}",
                truncate(task.trim(), 44),
                queue_count_suffix(self.queued_turns.len())
            );
            return;
        }

        let command = format!("/workflow {}", task.trim());
        let user_index = self.transcript.len();
        self.transcript
            .push(TranscriptItem::Message(ChatMessage::user(command.clone())));
        self.touch_transcript();
        self.persist_session();
        self.scroll_chat_to_bottom();

        let runtime = WorkflowRuntime::new(self.tools.workspace().to_path_buf())
            .with_memory_context(self.session_state_context_text());
        self.denied_this_turn.clear();
        self.denied_edits_this_turn.clear();
        let backend = self.model.clone();
        // Per-run checkpoint recorder + cancel token so subagent file edits are
        // captured (rewindable) and the run's tools are cancellable — parity
        // with the model-turn path.
        let recorder = self.new_workflow_checkpoint(&command, user_index);
        let cancel = CancelToken::new();
        let tools = self
            .tools
            .clone()
            .with_approval_handler(self.approval_handler.clone())
            .with_checkpoint_recorder(recorder.clone())
            .with_cancel_token(cancel.clone());
        #[cfg(test)]
        {
            self.last_workflow_runtime = Some(tools.clone());
        }
        let task = task.trim().to_string();
        let (sender, receiver) = mpsc::channel();
        self.workflow_events.push(BackgroundWorkflow {
            events: receiver,
            checkpoint: recorder,
            cancel,
        });
        if !self.is_working() {
            self.status_line = "background workflow starting".to_string();
        }
        self.toast("Background workflow started", ToastKind::Info);

        thread::spawn(move || {
            let result = runtime.run_task(task, backend, tools, |event| {
                sender.send(event).map_err(|error| {
                    color_eyre::eyre::eyre!("failed to send workflow event: {error}")
                })?;
                Ok(())
            });

            if let Err(error) = result {
                let _ = sender.send(WorkflowEvent::RunFinished {
                    run_id: "workflow-error".to_string(),
                    status: WorkflowStatus::Failed,
                    summary: format!("workflow failed: {error}"),
                });
            }
        });
    }

    fn start_workflow_script(&mut self, name: &str, raw_args: &str) {
        if !self.model_enabled {
            self.status_line = "workflow queued".to_string();
            return;
        }

        if self.is_working() {
            let queued = if raw_args.is_empty() {
                format!("/workflow {name}")
            } else {
                format!("/workflow {name} {raw_args}")
            };
            self.queued_turns.push_back(queued);
            self.status_line = format!(
                "queued workflow script: {name}{}",
                queue_count_suffix(self.queued_turns.len())
            );
            return;
        }

        let script = match WorkflowScript::load(self.tools.workspace(), name) {
            Ok(script) => script,
            Err(error) => {
                self.status_line = "workflow script failed to load".to_string();
                self.toast(format!("Workflow script error: {error}"), ToastKind::Error);
                return;
            }
        };

        let args = if raw_args.is_empty() {
            None
        } else {
            Some(
                serde_json::from_str(raw_args)
                    .unwrap_or_else(|_| serde_json::Value::String(raw_args.to_string())),
            )
        };

        let command = format!(
            "/workflow {name}{}{raw_args}",
            if raw_args.is_empty() { "" } else { " " }
        );
        let user_index = self.transcript.len();
        self.transcript
            .push(TranscriptItem::Message(ChatMessage::user(command.clone())));
        self.touch_transcript();
        self.persist_session();
        self.scroll_chat_to_bottom();

        let runtime = WorkflowRuntime::new(self.tools.workspace().to_path_buf())
            .with_memory_context(self.session_state_context_text());
        self.denied_this_turn.clear();
        self.denied_edits_this_turn.clear();
        let backend = self.model.clone();
        // Per-run checkpoint recorder + cancel token: see `start_workflow`.
        let recorder = self.new_workflow_checkpoint(&command, user_index);
        let cancel = CancelToken::new();
        let tools = self
            .tools
            .clone()
            .with_approval_handler(self.approval_handler.clone())
            .with_checkpoint_recorder(recorder.clone())
            .with_cancel_token(cancel.clone());
        #[cfg(test)]
        {
            self.last_workflow_runtime = Some(tools.clone());
        }
        let (sender, receiver) = mpsc::channel();
        self.workflow_events.push(BackgroundWorkflow {
            events: receiver,
            checkpoint: recorder,
            cancel,
        });
        self.status_line = format!("workflow script starting: {name}");
        self.toast("Workflow script started", ToastKind::Info);

        thread::spawn(move || {
            let result = runtime.run_script(&script, args, backend, tools, |event| {
                sender.send(event).map_err(|error| {
                    color_eyre::eyre::eyre!("failed to send workflow event: {error}")
                })?;
                Ok(())
            });

            if let Err(error) = result {
                let _ = sender.send(WorkflowEvent::RunFinished {
                    run_id: "workflow-error".to_string(),
                    status: WorkflowStatus::Failed,
                    summary: format!("workflow script failed: {error}"),
                });
            }
        });
    }

    fn drain_model_events(&mut self) -> bool {
        let Some(receiver) = self.model_events.take() else {
            return false;
        };

        let mut keep_receiver = true;
        let mut processed = 0usize;
        let mut turn_finished = false;
        let mut delta_buffer = String::new();
        let mut changed = false;

        while processed < 256 {
            match receiver.try_recv() {
                Ok(ModelStreamEvent::Delta(delta)) => {
                    changed = true;
                    delta_buffer.push_str(&delta);
                    self.status_line = self.scoped_status("streaming");
                }
                Ok(ModelStreamEvent::ReasoningDelta(delta)) => {
                    changed = true;
                    let _ = delta;
                    self.flush_stream_delta(&mut delta_buffer);
                    self.stick_chat_to_bottom_if_needed();
                }
                Ok(ModelStreamEvent::ToolStart {
                    call_id,
                    name,
                    summary,
                }) => {
                    changed = true;
                    self.flush_stream_delta(&mut delta_buffer);
                    self.stick_chat_to_bottom_if_needed();
                    self.streaming_message = None;
                    if name == "task.update" {
                        self.status_line = self.scoped_status(summary);
                    } else if name == "plan.update" {
                        self.status_line = self.scoped_status("updating plan");
                    } else if name == "decision.request" {
                        self.status_line = self.scoped_status("waiting on planning decision");
                    } else {
                        self.push_tool_start_with_id(Some(call_id), name.clone(), summary);
                        self.status_line = self.scoped_status(format!("running {name}"));
                    }
                    self.stick_chat_to_bottom_if_needed();
                }
                Ok(ModelStreamEvent::ToolResult {
                    call_id,
                    name,
                    output,
                }) => {
                    changed = true;
                    self.flush_stream_delta(&mut delta_buffer);
                    if name == "task.update" {
                        self.status_line = self.scoped_status(output);
                    } else if name == "plan.update" {
                        match self.apply_plan_update_output(&output) {
                            Ok(()) => {
                                self.status_line = self.scoped_status("plan updated");
                            }
                            Err(error) => {
                                self.push_tool_result(&name, format!("error: {error}"));
                                self.status_line = self.scoped_status("plan.update failed");
                            }
                        }
                    } else if name == "decision.request" {
                        match self.apply_decision_request_output(&output) {
                            Ok(()) => {
                                self.status_line = self.scoped_status("decision requested");
                            }
                            Err(error) => {
                                self.push_tool_result(&name, format!("error: {error}"));
                                self.status_line = self.scoped_status("decision.request failed");
                            }
                        }
                    } else {
                        // Parallel calls complete out of order; call_id pins
                        // the result to the right transcript block.
                        self.push_tool_result_for_call(&call_id, &name, output);
                        self.status_line = self.scoped_status(format!("{name} complete"));
                    }
                    self.stick_chat_to_bottom_if_needed();
                }
                Ok(ModelStreamEvent::Workflow(event)) => {
                    changed = true;
                    self.flush_stream_delta(&mut delta_buffer);
                    self.apply_workflow_event_from_model(event);
                    self.stick_chat_to_bottom_if_needed();
                }
                Ok(ModelStreamEvent::Usage(usage)) => {
                    // One event per model request; a tool-looping turn sends
                    // several, so sum them for turn and session totals.
                    changed = true;
                    self.turn_usage.add(usage);
                    self.turn_requests += 1;
                    self.session_usage.add(usage);
                    self.session_requests += 1;
                }
                Ok(ModelStreamEvent::Done { event_count }) => {
                    changed = true;
                    self.flush_stream_delta(&mut delta_buffer);
                    if self.cancel_requested_at.is_some() {
                        // Esc raced the natural finish and the user asked to
                        // stop: honor the stop intent. Finalize as an
                        // interruption (which keeps queued prompts per [21])
                        // and — crucially — do NOT set `turn_finished`, so the
                        // tail never auto-launches the next queued turn. A
                        // cancel intent must never silently start more work.
                        self.finalize_cancelled_turn("turn interrupted");
                        keep_receiver = false;
                        break;
                    }
                    self.record_turn_usage_totals();
                    self.status_line =
                        self.scoped_status(format!("complete ({event_count} events)"));
                    self.stick_chat_to_bottom_if_needed();
                    self.streaming_message = None;
                    self.ring_bell_if_due();
                    self.turn_started_at.take();
                    self.turn_cancel = None;
                    keep_receiver = false;
                    turn_finished = true;
                    break;
                }
                Ok(ModelStreamEvent::Cancelled) => {
                    changed = true;
                    self.flush_stream_delta(&mut delta_buffer);
                    self.finalize_cancelled_turn("turn interrupted");
                    keep_receiver = false;
                    break;
                }
                // Post-cancel socket/send errors are fallout from the user's
                // own Esc — render them as the interruption they are, never
                // as a scary failure toast.
                Ok(ModelStreamEvent::Error(_)) if self.cancel_requested_at.is_some() => {
                    changed = true;
                    self.flush_stream_delta(&mut delta_buffer);
                    self.finalize_cancelled_turn("turn interrupted");
                    keep_receiver = false;
                    break;
                }
                Ok(ModelStreamEvent::Error(error)) => {
                    changed = true;
                    self.flush_stream_delta(&mut delta_buffer);
                    self.record_turn_usage_totals();
                    self.ring_bell_if_due();
                    let clean_error = clean_model_error(&error);
                    if let Some(index) = self.streaming_message {
                        if let Some(TranscriptItem::Message(message)) =
                            self.transcript.get_mut(index)
                        {
                            if !message.content.is_empty() {
                                message.content.push('\n');
                            }
                            message.content.push_str(&clean_error);
                            self.touch_transcript();
                        }
                    } else {
                        self.transcript
                            .push(TranscriptItem::Message(ChatMessage::system(
                                clean_error.clone(),
                            )));
                        self.touch_transcript();
                    }
                    self.persist_session();
                    self.status_line = model_error_status(&clean_error).to_string();
                    self.toast(clean_error, ToastKind::Error);
                    self.streaming_message = None;
                    self.turn_cancel = None;
                    keep_receiver = false;
                    turn_finished = true;
                    break;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    changed = true;
                    self.flush_stream_delta(&mut delta_buffer);
                    if self.cancel_requested_at.is_some() {
                        // Worker died mid-cancel without a final event.
                        self.finalize_cancelled_turn("turn interrupted");
                        keep_receiver = false;
                        break;
                    }
                    if self.streaming_message.is_some() {
                        self.status_line = self.scoped_status("stream ended");
                    }
                    self.record_turn_usage_totals();
                    self.ring_bell_if_due();
                    self.streaming_message = None;
                    self.turn_cancel = None;
                    keep_receiver = false;
                    turn_finished = true;
                    break;
                }
            }

            processed += 1;
        }

        self.flush_stream_delta(&mut delta_buffer);
        self.stick_chat_to_bottom_if_needed();

        if self.streaming_message.is_some()
            && self.last_stream_save.elapsed() >= Duration::from_millis(750)
        {
            self.persist_session();
            self.last_stream_save = Instant::now();
        }

        if keep_receiver {
            self.model_events = Some(receiver);
        } else if turn_finished {
            self.finish_turn_checkpoint();
            self.start_next_queued_turn();
        }

        changed
    }

    /// Build a per-run checkpoint recorder for a background workflow, keyed to
    /// the workflow command's user-message row. Mirrors the model-turn recorder
    /// so `/rewind` treats workflow edits exactly like model-turn edits.
    fn new_workflow_checkpoint(&self, command: &str, user_index: usize) -> CheckpointRecorder {
        CheckpointRecorder::new(
            self.tools.workspace(),
            CheckpointMeta {
                session_id: self
                    .session
                    .as_ref()
                    .map(SessionStore::current_id)
                    .unwrap_or_default(),
                prompt_excerpt: excerpt_for_checkpoint(command),
                transcript_user_index: user_index,
            },
        )
    }

    /// Close out a checkpoint recorder (model turn or workflow): a quiet
    /// status-line note when files were captured (never a transcript row),
    /// then retention pruning. Returns the summary when anything was captured.
    fn finalize_checkpoint(&mut self, recorder: &CheckpointRecorder) -> Option<CheckpointSummary> {
        let summary = recorder.finish()?;
        self.status_line = format!(
            "checkpoint · {} file{}",
            summary.file_count,
            if summary.file_count == 1 { "" } else { "s" }
        );
        if let Ok(store) = CheckpointStore::open(self.tools.workspace())
            && let Err(error) = store.prune(RetentionLimits::from_env())
        {
            self.status_line = format!("checkpoint prune failed: {error}");
        }
        Some(summary)
    }

    fn finish_turn_checkpoint(&mut self) {
        let Some(recorder) = self.active_checkpoint.take() else {
            return;
        };
        self.finalize_checkpoint(&recorder);
    }

    fn drain_workflow_events(&mut self) -> bool {
        if self.workflow_events.is_empty() {
            return false;
        };

        let workflows = std::mem::take(&mut self.workflow_events);
        let mut active_workflows = Vec::new();
        let mut any_finished = false;
        let mut changed = false;

        for workflow in workflows {
            let mut keep_receiver = true;
            let mut processed = 0usize;
            let mut finished = false;

            while processed < 256 {
                match workflow.events.try_recv() {
                    Ok(event) => {
                        changed = true;
                        finished |= self.apply_workflow_event(event);
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        changed = true;
                        keep_receiver = false;
                        finished = true;
                        if !self.is_working()
                            && (self.status_line == "background workflow starting"
                                || self.status_line.contains("workflow"))
                        {
                            self.status_line = "background workflow ended".to_string();
                        }
                        break;
                    }
                }

                processed += 1;
            }

            if keep_receiver && !finished {
                active_workflows.push(workflow);
            } else if finished {
                any_finished = true;
                // The worker thread is gone: close out its checkpoint so the
                // captured pre-images are pruned like a model turn's. The
                // manifest was already written on each capture, so /rewind can
                // undo the run even if this prune never ran.
                self.finalize_checkpoint(&workflow.checkpoint);
            }
        }

        self.workflow_events = active_workflows;
        if any_finished {
            self.persist_session();
            // A background workflow finishing must also drain the queue —
            // otherwise turns queued while it ran strand forever (the only
            // other dequeue site is a model turn's completion). Start the next
            // queued turn only when the app is fully idle: no streaming model
            // turn and no remaining background workflow.
            if !self.is_working() && !self.has_active_workflows() {
                self.start_next_queued_turn();
            }
        }

        changed
    }

    fn drain_background_job_events(&mut self) -> bool {
        let mut processed = 0usize;
        let mut changed = false;
        while processed < 128 {
            match self.background_job_events.try_recv() {
                Ok(event) => {
                    changed = true;
                    self.apply_background_job_event(event);
                    processed += 1;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }

        changed
    }

    fn drain_approval_requests(&mut self) -> bool {
        let mut changed = false;
        let mut processed = 0usize;
        while processed < 128 {
            match self.approval_events.try_recv() {
                Ok(pending) => {
                    changed = true;
                    processed += 1;
                    if self.cancel_requested_at.is_some() {
                        // In-flight approval raced the cancel: deny it so the
                        // parked worker unblocks and sees the token.
                        let _ = pending.respond.send(ApprovalDecision::Deny);
                    } else if let Some(decision) = self.auto_approval_decision(&pending.request) {
                        let _ = pending.respond.send(decision);
                    } else {
                        self.approval_queue.push_back(pending);
                    }
                }
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }

        if changed && !self.approval_queue.is_empty() {
            if self.approval_shown_at.is_none() {
                self.approval_shown_at = Some(Instant::now());
                self.ring_bell_if_due();
            }
            let front = &self.approval_queue[0].request;
            self.status_line = format!("approval required: {}", front.tool.label());
        }
        changed
    }

    /// Session memory: grants approved with "always allow" this session and
    /// exact commands already denied this turn resolve without prompting.
    fn auto_approval_decision(&self, request: &ApprovalRequest) -> Option<ApprovalDecision> {
        match request.tool {
            ApprovalTool::TerminalExec => {
                let command = request.command.as_deref().unwrap_or("").trim();
                if self.denied_this_turn.iter().any(|denied| denied == command) {
                    return Some(ApprovalDecision::Deny);
                }
                // Sandbox escalations always need a fresh human decision:
                // a stored grant covers running the command, never running
                // it outside the sandbox.
                if request.sandbox_escalation {
                    return None;
                }
                // Match grants against the command with leading env
                // assignments stripped, so a grant on `cargo build` still
                // settles `FOO=bar cargo build`.
                let effective = strip_env_assignments(command);
                if !command_has_shell_tokens(command)
                    && self
                        .session_terminal_grants
                        .iter()
                        .any(|prefix| command_matches_grant(effective, prefix))
                {
                    return Some(ApprovalDecision::AllowOnce);
                }
                None
            }
            ApprovalTool::FileEdit | ApprovalTool::FilePatch => {
                if !request.paths.is_empty()
                    && request
                        .paths
                        .iter()
                        .all(|path| self.denied_edits_this_turn.iter().any(|p| p == path))
                {
                    return Some(ApprovalDecision::Deny);
                }
                let all_granted = !request.paths.is_empty()
                    && request
                        .paths
                        .iter()
                        .all(|path| edit_grant_matches(&self.session_edit_grants, path));
                all_granted.then_some(ApprovalDecision::AllowOnce)
            }
            ApprovalTool::McpTool | ApprovalTool::McpServerLaunch => {
                // Per-tool always-allow grants (and per-session launch
                // approvals) live in the core registry, which skips the gate
                // entirely once granted; here we only auto-deny an identical
                // request re-asked in the same turn after the user said no.
                let command = request.command.as_deref().unwrap_or("").trim();
                if self.denied_this_turn.iter().any(|denied| denied == command) {
                    return Some(ApprovalDecision::Deny);
                }
                None
            }
            ApprovalTool::WebFetch | ApprovalTool::WebSearch => {
                // Once the user always-allows web egress this session, every
                // later fetch/search resolves silently; otherwise re-asked
                // denials are honoured and anything new prompts.
                if self.session_web_egress_allowed {
                    return Some(ApprovalDecision::AllowOnce);
                }
                let command = request.command.as_deref().unwrap_or("").trim();
                if self.denied_this_turn.iter().any(|denied| denied == command) {
                    return Some(ApprovalDecision::Deny);
                }
                None
            }
        }
    }

    fn resolve_pending_approval(&mut self, decision: ApprovalDecision) {
        let Some(pending) = self.approval_queue.pop_front() else {
            return;
        };
        // The next queued request must serve its own grace window.
        self.approval_shown_at = None;

        // Defense in depth: even if an always-allow decision reaches an
        // escalation (the card doesn't offer one), downgrade it to a
        // one-shot approval so nothing is persisted.
        let decision =
            if pending.request.sandbox_escalation && decision == ApprovalDecision::AlwaysAllow {
                ApprovalDecision::AllowOnce
            } else {
                decision
            };

        match decision {
            ApprovalDecision::AlwaysAllow => self.record_always_allow(&pending.request),
            ApprovalDecision::Deny => {
                if let Some(command) = pending.request.command.as_deref() {
                    self.denied_this_turn.push(command.trim().to_string());
                }
                for path in &pending.request.paths {
                    self.denied_edits_this_turn.push(path.clone());
                }
            }
            ApprovalDecision::AllowOnce => {}
        }

        let _ = pending.respond.send(decision);
        self.status_line = match decision {
            ApprovalDecision::AllowOnce => "approved once".to_string(),
            ApprovalDecision::AlwaysAllow => "always allowed".to_string(),
            ApprovalDecision::Deny => "denied".to_string(),
        };

        // A grant can settle other queued requests immediately (bursts from
        // parallel subagents asking for the same thing).
        let mut remaining = std::mem::take(&mut self.approval_queue);
        while let Some(pending) = remaining.pop_front() {
            if let Some(auto) = self.auto_approval_decision(&pending.request) {
                let _ = pending.respond.send(auto);
            } else {
                self.approval_queue.push_back(pending);
            }
        }
    }

    fn record_always_allow(&mut self, request: &ApprovalRequest) {
        match request.tool {
            ApprovalTool::TerminalExec => {
                let Some(command) = request.command.as_deref() else {
                    return;
                };
                let Some(prefix) = derive_terminal_grant_prefix(command) else {
                    // Complex commands only get allow-once semantics.
                    return;
                };
                self.session_terminal_grants.push(prefix.clone());
                match medusa_core::permissions::PermissionPolicy::append_terminal_allow_prefix(
                    self.tools.workspace(),
                    &prefix,
                ) {
                    Ok(()) => {
                        // Reload so future turns see the persisted grant.
                        if let Ok(reloaded) = ToolRuntime::new(self.tools.workspace()) {
                            self.tools = reloaded.with_mcp(self.mcp.clone());
                        }
                        self.toast(format!("Always allowing `{prefix}`"), ToastKind::Success);
                    }
                    Err(error) => {
                        self.toast(format!("Grant not persisted: {error}"), ToastKind::Warning);
                    }
                }
            }
            ApprovalTool::FileEdit | ApprovalTool::FilePatch => {
                for path in &request.paths {
                    let prefix = path
                        .rsplit_once('/')
                        .map(|(dir, _)| format!("{dir}/"))
                        .unwrap_or_else(|| path.clone());
                    if !self.session_edit_grants.contains(&prefix) {
                        self.session_edit_grants.push(prefix);
                    }
                }
                self.toast(
                    "Always allowing edits there this session",
                    ToastKind::Success,
                );
            }
            ApprovalTool::McpTool => {
                // The core registry recorded the per-(server, tool) grant when
                // authorize returned "always allow"; nothing is persisted to
                // disk for MCP in v1.
                self.toast(
                    "Always allowing that MCP tool this session",
                    ToastKind::Success,
                );
            }
            ApprovalTool::McpServerLaunch => {
                // The core registry marked the server launch-approved for the
                // session; the process is spawned on first use.
                self.toast(
                    "Allowing that MCP server to launch this session",
                    ToastKind::Success,
                );
            }
            ApprovalTool::WebFetch | ApprovalTool::WebSearch => {
                self.session_web_egress_allowed = true;
                self.toast("Allowing web requests this session", ToastKind::Success);
            }
        }
    }

    fn apply_background_job_event(&mut self, event: BackgroundJobEvent) {
        match event {
            BackgroundJobEvent::Started {
                id,
                pid,
                command,
                cwd,
            } => {
                self.background_jobs.insert(
                    id.clone(),
                    BackgroundJobView {
                        id: id.clone(),
                        pid,
                        command: command.clone(),
                        cwd,
                        state: ToolRunState::Running,
                        started_at: Instant::now(),
                        finished_at: None,
                        exit_code: None,
                        last_output: String::new(),
                    },
                );
                self.attach_or_push_background_tool_start(&id, &command);
                self.update_tool_result_by_id(
                    &id,
                    ToolRunState::Running,
                    &format!("running · pid {pid}\ncommand: {command}"),
                );
                self.status_line = format!("background shell running · pid {pid}");
                self.toast(self.status_line.clone(), ToastKind::Info);
            }
            BackgroundJobEvent::Finished {
                id,
                pid,
                command,
                cwd,
                code,
                stdout,
                stderr,
            } => {
                let state = if code == Some(0) {
                    ToolRunState::Succeeded
                } else {
                    ToolRunState::Failed
                };
                let detail = compact_tool_detail(&terminal_result_output(&TerminalExecResult {
                    command: command.clone(),
                    cwd,
                    code,
                    stdout,
                    stderr,
                    background: false,
                    pid: Some(pid),
                    job_id: Some(id.clone()),
                    sandboxed: false,
                }));
                if let Some(job) = self.background_jobs.get_mut(&id) {
                    job.state = state;
                    job.finished_at = Some(Instant::now());
                    job.exit_code = code;
                    job.last_output = detail.clone();
                }
                self.update_tool_result_by_id(&id, state, &detail);
                self.status_line = format!(
                    "background shell completed · pid {pid} · exit {}",
                    code.unwrap_or(-1)
                );
                self.toast(self.status_line.clone(), ToastKind::Success);
                self.persist_session();
            }
            BackgroundJobEvent::Failed {
                id,
                pid,
                command,
                error,
                ..
            } => {
                let detail = compact_tool_detail(&format!("command: {command}\nerror: {error}"));
                if let Some(job) = self.background_jobs.get_mut(&id) {
                    job.state = ToolRunState::Failed;
                    job.finished_at = Some(Instant::now());
                    job.last_output = detail.clone();
                }
                self.update_tool_result_by_id(&id, ToolRunState::Failed, &detail);
                self.status_line = format!("background shell failed · pid {pid}");
                self.toast(self.status_line.clone(), ToastKind::Error);
                self.persist_session();
            }
        }
        self.stick_chat_to_bottom_if_needed();
    }

    fn apply_workflow_event(&mut self, event: WorkflowEvent) -> bool {
        self.apply_workflow_event_inner(event, true)
    }

    /// Workflow events from a model-launched `workflow_run` tool call: update
    /// the tree but skip the final assistant summary message — the model
    /// receives the result as a tool output and reports it in its own words.
    fn apply_workflow_event_from_model(&mut self, event: WorkflowEvent) {
        self.apply_workflow_event_inner(event, false);
    }

    fn apply_workflow_event_inner(&mut self, event: WorkflowEvent, announce_summary: bool) -> bool {
        match event {
            WorkflowEvent::RunStarted {
                run_id,
                title,
                task,
                phases,
            } => {
                let view = workflow_view_from_plan(run_id, title, task, phases);
                self.set_workflow_status_line(format!("workflow: {}", truncate(&view.title, 48)));
                self.workflows.push(view.clone());
                self.transcript.push(TranscriptItem::Workflow(view));
                self.touch_transcript();
                self.stick_chat_to_bottom_if_needed();
                self.persist_session();
                false
            }
            WorkflowEvent::PhaseStarted {
                run_id,
                phase_index,
                name,
                ..
            } => {
                self.update_workflow(&run_id, |workflow| {
                    workflow.status = WorkflowViewState::Running;
                    // Script workflows create phases dynamically, so unseen
                    // indexes are appended rather than ignored.
                    while workflow.phases.len() <= phase_index {
                        workflow.phases.push(WorkflowPhaseView {
                            name: name.clone(),
                            objective: String::new(),
                            status: WorkflowViewState::Pending,
                            agents: Vec::new(),
                        });
                    }
                    if let Some(phase) = workflow.phases.get_mut(phase_index) {
                        phase.name = name.clone();
                        phase.status = WorkflowViewState::Running;
                    }
                });
                self.set_workflow_status_line(format!("workflow phase: {name}"));
                self.stick_chat_to_bottom_if_needed();
                false
            }
            WorkflowEvent::AgentStarted {
                run_id,
                phase_index,
                agent_index,
                name,
                role,
                tool_policy,
            } => {
                self.update_workflow(&run_id, |workflow| {
                    workflow.status = WorkflowViewState::Running;
                    if let Some(phase) = workflow.phases.get_mut(phase_index) {
                        while phase.agents.len() <= agent_index {
                            phase.agents.push(WorkflowAgentView {
                                name: name.clone(),
                                role: role.clone(),
                                tool_policy,
                                status: WorkflowViewState::Pending,
                                output: String::new(),
                                tool_counts: BTreeMap::new(),
                            });
                        }
                        if let Some(agent) = phase.agents.get_mut(agent_index) {
                            agent.name = name.clone();
                            agent.role = role.clone();
                            agent.tool_policy = tool_policy;
                            agent.status = WorkflowViewState::Running;
                        }
                    }
                });
                self.set_workflow_status_line(format!("subagent: {name}"));
                self.stick_chat_to_bottom_if_needed();
                false
            }
            WorkflowEvent::AgentFinished {
                run_id,
                phase_index,
                agent_index,
                name,
                status,
                output,
                tool_counts,
            } => {
                let state = workflow_state_from_core(status);
                self.update_workflow(&run_id, |workflow| {
                    if let Some(agent) = workflow
                        .phases
                        .get_mut(phase_index)
                        .and_then(|phase| phase.agents.get_mut(agent_index))
                    {
                        agent.status = state;
                        agent.output = compact_tool_detail(&output);
                        agent.tool_counts = tool_counts.clone();
                    }
                });
                self.set_workflow_status_line(format!("subagent complete: {name}"));
                self.stick_chat_to_bottom_if_needed();
                false
            }
            WorkflowEvent::PhaseFinished {
                run_id,
                phase_index,
                name,
                status,
            } => {
                let state = workflow_state_from_core(status);
                self.update_workflow(&run_id, |workflow| {
                    if let Some(phase) = workflow.phases.get_mut(phase_index) {
                        phase.status = state;
                    }
                });
                self.set_workflow_status_line(format!("workflow phase complete: {name}"));
                self.stick_chat_to_bottom_if_needed();
                false
            }
            WorkflowEvent::Log { run_id, message } => {
                let _ = run_id;
                self.set_workflow_status_line(format!("workflow: {message}"));
                false
            }
            WorkflowEvent::RunFinished {
                run_id,
                status,
                summary,
            } => {
                let state = workflow_state_from_core(status);
                self.update_workflow(&run_id, |workflow| {
                    workflow.status = state;
                    workflow.summary = summary.clone();
                });
                if announce_summary {
                    self.transcript
                        .push(TranscriptItem::Message(ChatMessage::assistant(
                            summary.clone(),
                        )));
                    self.touch_transcript();
                }
                let status_line = match status {
                    WorkflowStatus::Succeeded => "workflow complete".to_string(),
                    WorkflowStatus::PartiallySucceeded => "workflow partially complete".to_string(),
                    WorkflowStatus::Running => "workflow running".to_string(),
                    WorkflowStatus::Failed => "workflow failed".to_string(),
                };
                self.set_workflow_status_line(status_line.clone());
                self.toast(
                    status_line,
                    match status {
                        WorkflowStatus::Failed => ToastKind::Error,
                        WorkflowStatus::PartiallySucceeded => ToastKind::Warning,
                        WorkflowStatus::Running | WorkflowStatus::Succeeded => ToastKind::Success,
                    },
                );
                self.stick_chat_to_bottom_if_needed();
                self.persist_session();
                true
            }
        }
    }

    fn update_workflow(&mut self, run_id: &str, mut update: impl FnMut(&mut WorkflowRunView)) {
        for workflow in &mut self.workflows {
            if workflow.id == run_id {
                update(workflow);
            }
        }

        let mut transcript_changed = false;
        for item in &mut self.transcript {
            if let TranscriptItem::Workflow(workflow) = item
                && workflow.id == run_id
            {
                update(workflow);
                transcript_changed = true;
            }
        }

        if transcript_changed {
            self.touch_transcript();
        }
        self.persist_session();
    }

    fn start_next_queued_turn(&mut self) {
        let Some(task) = self.queued_turns.pop_front() else {
            return;
        };

        if let Some(workflow_task) = task.strip_prefix("/workflow ") {
            self.status_line = "starting queued workflow".to_string();
            self.start_workflow(workflow_task);
            return;
        }

        self.transcript
            .push(TranscriptItem::Message(ChatMessage::user(task.clone())));
        self.touch_transcript();
        self.persist_session();
        self.scroll_chat_to_bottom();
        self.status_line = if self.queued_turns.is_empty() {
            "starting queued turn".to_string()
        } else {
            format!("starting queued turn · {} waiting", self.queued_turns.len())
        };
        self.start_model_turn(&task);
    }

    fn flush_stream_delta(&mut self, delta_buffer: &mut String) {
        if delta_buffer.is_empty() {
            return;
        }

        if let Some(index) = self.streaming_message {
            if let Some(TranscriptItem::Message(message)) = self.transcript.get_mut(index) {
                message.content.push_str(delta_buffer);
                self.touch_transcript();
            }
        } else {
            let index = self.transcript.len();
            self.transcript
                .push(TranscriptItem::Message(ChatMessage::assistant(
                    std::mem::take(delta_buffer),
                )));
            self.touch_transcript();
            self.streaming_message = Some(index);
            return;
        }

        delta_buffer.clear();
        self.stick_chat_to_bottom_if_needed();
        delta_buffer.clear();
    }

    fn push_tool_start(&mut self, name: String, summary: String) {
        self.push_tool_start_with_id(None, name, summary);
    }

    fn push_tool_start_with_id(&mut self, id: Option<String>, name: String, summary: String) {
        let run = ToolRun {
            id,
            started_at: Instant::now(),
            pending_result: None,
            name,
            summary,
            state: ToolRunState::Running,
            detail: String::new(),
            expanded: false,
            group_expanded: false,
        };
        self.transcript.push(TranscriptItem::Tool(run));
        self.touch_transcript();
        self.persist_session();
    }

    fn push_tool_result(&mut self, name: &str, output: String) {
        let state = if tool_output_failed(&output) {
            ToolRunState::Failed
        } else {
            ToolRunState::Succeeded
        };
        let detail = compact_tool_detail(&output);
        self.update_transcript_tool_result(name, state, &detail);
        self.persist_session();
    }

    /// Resolve a tool result to its transcript block by call id — required for
    /// parallel calls, where two same-named runs can be in flight at once and
    /// "most recent running with this name" would misattribute results.
    fn push_tool_result_for_call(&mut self, call_id: &str, name: &str, output: String) {
        let state = if tool_output_failed(&output) {
            ToolRunState::Failed
        } else {
            ToolRunState::Succeeded
        };
        let detail = compact_tool_detail(&output);

        if let Some(run) = self
            .transcript
            .iter_mut()
            .rev()
            .find_map(|item| match item {
                TranscriptItem::Tool(run)
                    if run.id.as_deref() == Some(call_id) && run.state == ToolRunState::Running =>
                {
                    Some(run)
                }
                _ => None,
            })
        {
            queue_or_apply_tool_result(run, state, detail, state == ToolRunState::Failed);
            self.touch_transcript();
            self.persist_session();
            return;
        }

        // No started block carries this id (e.g. restored session) — fall
        // back to the name-based path, which also creates a block if needed.
        self.update_transcript_tool_result(name, state, &detail);
        self.persist_session();
    }

    fn update_transcript_tool_result(&mut self, name: &str, state: ToolRunState, detail: &str) {
        if let Some(run) = self
            .transcript
            .iter_mut()
            .rev()
            .find_map(|item| match item {
                TranscriptItem::Tool(run)
                    if run.name == name && run.state == ToolRunState::Running =>
                {
                    Some(run)
                }
                _ => None,
            })
        {
            queue_or_apply_tool_result(run, state, detail.to_string(), false);
            self.touch_transcript();
            return;
        }

        self.transcript.push(TranscriptItem::Tool(ToolRun {
            id: None,
            started_at: Instant::now(),
            pending_result: None,
            name: name.to_string(),
            summary: String::new(),
            state,
            detail: detail.to_string(),
            expanded: false,
            group_expanded: false,
        }));
        self.touch_transcript();
    }

    fn conversation_history(&self) -> Vec<ConversationMessage> {
        let mut messages = Vec::new();
        messages.push(ConversationMessage {
            role: "system".to_string(),
            content: permission_context_text(self.permission_mode).to_string(),
            attachments: Vec::new(),
        });
        messages.push(ConversationMessage {
            role: "system".to_string(),
            content: self.session_state_context_text(),
            attachments: Vec::new(),
        });
        if self.plan_mode {
            messages.push(ConversationMessage {
                role: "system".to_string(),
                content: PLAN_MODE_DIRECTIVE.to_string(),
                attachments: Vec::new(),
            });
        }

        messages.extend(self.recent_conversation_messages());
        messages
    }

    /// Full conversation history; token budgeting and compaction happen in
    /// the ContextEngine at turn start, not by windowing here.
    fn recent_conversation_messages(&self) -> Vec<ConversationMessage> {
        self.transcript
            .iter()
            .filter_map(transcript_conversation_message)
            .collect()
    }

    fn session_state_context_text(&self) -> String {
        session_state_context_text(
            &self.transcript,
            self.recent_conversation_messages().len(),
            SessionStateRuntime {
                workspace: &self.cwd_display,
                model: self.model.model_name(),
                permission_mode: self.permission_mode,
                status: &self.status_line,
                workflows: &self.workflows,
                active_workflows: self.workflow_events.len(),
                background_jobs: &self.background_jobs,
            },
        )
    }

    fn persist_session(&mut self) {
        if let Some(session) = &self.session
            && let Err(error) = session.save_transcript(&self.transcript)
        {
            self.status_line = format!("session save failed: {error}");
        }
    }

    fn toast(&mut self, message: impl Into<String>, kind: ToastKind) {
        self.toast = Some(Toast {
            message: message.into(),
            kind,
            created_at: Instant::now(),
        });
    }

    fn animation_frame(&self) -> u64 {
        // Keep the working indicator feeling active, not stuck.
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| (duration.as_millis() / 110) as u64)
    }

    fn expire_toast(&mut self) -> bool {
        if self
            .toast
            .as_ref()
            .is_some_and(|toast| toast.created_at.elapsed() > Duration::from_secs(3))
        {
            self.toast = None;
            return true;
        }

        false
    }

    fn draw(&mut self, frame: &mut Frame<'_>) {
        let area = frame.area();
        frame.render_widget(
            Block::default().style(Style::default().bg(app_bg()).fg(text())),
            area,
        );

        let shell_area = area.inner(Margin {
            horizontal: 1,
            vertical: 0,
        });
        let input_height = self.input_height(area.height);
        let plan_height = self.plan_strip_height(area.height);
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Min(5),
                Constraint::Length(plan_height),
                Constraint::Length(input_height),
                Constraint::Length(1),
            ])
            .split(shell_area);

        self.draw_header(frame, sections[0]);
        self.draw_workspace(frame, sections[1]);
        self.draw_plan_strip(frame, sections[2]);
        self.draw_input(frame, sections[3]);
        self.draw_status(frame, sections[4]);
        if self.active_modal.is_none() {
            self.draw_slash_suggestions(frame, shell_area);
            self.draw_mention_suggestions(frame, shell_area);
        }
        self.draw_modal(frame, shell_area);
        self.draw_approval_prompt(frame, shell_area);
    }

    fn focus(&self) -> UiFocus {
        if self.active_modal.is_some() {
            UiFocus::Modal
        } else if self.selected_tool.is_some() {
            UiFocus::Activity
        } else if self.chat_scroll > 0 {
            UiFocus::Transcript
        } else {
            UiFocus::Composer
        }
    }

    fn draw_workspace(&mut self, frame: &mut Frame<'_>, area: Rect) {
        self.draw_messages(frame, area);
    }

    fn draw_header(&self, frame: &mut Frame<'_>, area: Rect) {
        if area.height == 0 {
            return;
        }

        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Length(1)])
            .split(area);
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(56), Constraint::Length(36)])
            .split(sections[0]);
        let (state_label, state_style) = self.header_state();
        let session = self
            .session
            .as_ref()
            .map(|session| compact_session_id(&session.current_id()))
            .unwrap_or_else(|| "no-session".to_string());
        let left = Paragraph::new(Line::from(vec![
            Span::styled(
                " MEDUSA ",
                Style::default()
                    .fg(palette().selected_fg)
                    .bg(accent_color())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("  ", muted()),
            Span::styled("● ", state_style),
            Span::styled(state_label, state_style),
            Span::styled("  workspace ", muted()),
            Span::styled(self.cwd_display.clone(), value_style()),
        ]))
        .style(Style::default().bg(surface()).fg(text()));
        // Left side: state + workspace
        frame.render_widget(left, columns[0]);

        // Right side: perm / git / session info
        {
            let mut right_spans = vec![];
            right_spans.push(Span::styled("perm ", muted()));
            right_spans.push(Span::styled(self.permission_mode.name(), value_style()));
            right_spans.push(Span::styled("  ", muted()));
            right_spans.push(Span::styled(
                if self.inside_git_repo {
                    "git"
                } else {
                    "no-git"
                },
                if self.inside_git_repo {
                    success_style()
                } else {
                    muted()
                },
            ));
            right_spans.push(Span::styled("  ", muted()));
            right_spans.push(Span::styled(session, muted()));
            let right = Paragraph::new(Line::from(right_spans))
                .alignment(Alignment::Right)
                .style(Style::default().bg(surface()).fg(text()));
            frame.render_widget(right, columns[1]);
        }

        let rule = Paragraph::new(Line::from(Span::styled(
            "─".repeat(sections[1].width as usize),
            separator_style(),
        )))
        .style(Style::default().bg(surface()));
        frame.render_widget(rule, sections[1]);
    }

    fn header_state(&self) -> (&'static str, Style) {
        if self.is_working() {
            ("working", tool_label_style())
        } else if self.has_active_workflows() {
            ("workflow", prompt_style())
        } else if self.has_running_tool_rows() {
            ("tools", tool_label_style())
        } else if self.permission_mode == PermissionMode::Readonly {
            ("readonly", prompt_style())
        } else if self.permission_mode == PermissionMode::Guarded {
            ("guarded", prompt_style())
        } else {
            ("ready", success_style())
        }
    }

    fn draw_messages(&mut self, frame: &mut Frame<'_>, area: Rect) {
        self.last_chat_viewport = Some(area);
        let rows = self.visible_transcript_rows_cached();
        let metrics = chat_viewport_metrics(&rows, area, self.chat_scroll);
        self.chat_scroll = metrics.scroll;
        let window = transcript_viewport_window(
            &rows,
            metrics.text_area.width,
            metrics.top_offset,
            metrics.text_area.height as usize,
        );
        let chat_lines = transcript_lines_from_rows(&window.rows);

        let chat = Paragraph::new(chat_lines)
            .style(Style::default().bg(surface()))
            .wrap(Wrap { trim: false })
            .scroll((paragraph_scroll_offset(window.scroll_offset), 0));
        frame.render_widget(chat, metrics.text_area);
        self.render_transcript_images(frame, metrics.text_area, &rows, metrics.top_offset);
        self.draw_transcript_scroll_thumb(frame, area, metrics);
        self.last_transcript_rows = rows;
    }

    fn draw_transcript_scroll_thumb(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        metrics: ChatViewportMetrics,
    ) {
        if !metrics.has_scrollbar || metrics.scroll == 0 || area.width == 0 || area.height == 0 {
            return;
        }

        let mut state = ScrollbarState::new(metrics.max_scroll.max(1)).position(metrics.top_offset);
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .track_symbol(Some(" "))
            .thumb_symbol("▌")
            .track_style(Style::default().bg(surface()))
            .thumb_style(accent().bg(surface()));
        frame.render_stateful_widget(scrollbar, area, &mut state);
    }

    fn visible_transcript_rows(&self) -> Vec<TranscriptRow> {
        visible_transcript_rows(
            &self.transcript,
            self.streaming_message,
            self.selected_tool,
            RenderContext {
                animation_tick: self.animation_tick,
                decision_selection: self.decision_selection,
            },
        )
    }

    fn visible_transcript_rows_cached(&mut self) -> Arc<Vec<TranscriptRow>> {
        let animation_tick = if self.has_running_tool_rows() || self.has_running_workflow_rows() {
            Some(self.animation_tick)
        } else {
            None
        };
        if let Some(cache) = &self.transcript_rows_cache
            && cache.version == self.transcript_version
            && cache.theme == self.theme
            && cache.streaming_message == self.streaming_message
            && cache.selected_tool == self.selected_tool
            && cache.animation_tick == animation_tick
            && cache.decision_selection == self.decision_selection
        {
            return Arc::clone(&cache.rows);
        }

        let rows = Arc::new(self.visible_transcript_rows());
        self.transcript_rows_cache = Some(TranscriptRowsCache {
            version: self.transcript_version,
            theme: self.theme,
            streaming_message: self.streaming_message,
            selected_tool: self.selected_tool,
            animation_tick,
            decision_selection: self.decision_selection,
            rows: Arc::clone(&rows),
        });
        rows
    }

    fn render_transcript_images(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        rows: &[TranscriptRow],
        top_offset: usize,
    ) {
        if area.width < 8 || area.height == 0 {
            return;
        }

        for placement in transcript_image_placements(rows, area, top_offset) {
            self.image_renderer.render(
                frame,
                &placement.attachment,
                area,
                placement.width,
                placement.height,
                placement.x_offset,
                placement.y_offset,
            );
        }
    }

    fn draw_input(&self, frame: &mut Frame<'_>, area: Rect) {
        let mut display = Vec::new();
        if !self.pending_attachments.is_empty() {
            display.push(attachment_strip_line(&self.pending_attachments));
            display.extend(composer_attachment_preview_lines(
                &self.pending_attachments,
                &self.attachment_previews,
                area.width,
            ));
        }
        display.extend(input_display_lines(
            &self.input,
            self.input_cursor,
            area.height.saturating_sub(2) as usize,
        ));
        display = vertically_center_input_lines(display, area.height.saturating_sub(2));
        let border_style = if self.model_events.is_some() {
            muted()
        } else {
            Style::default().fg(accent_color())
        };

        let input = Paragraph::new(display)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .title(self.input_title_content())
                    .border_style(border_style)
                    .style(Style::default().bg(surface()).fg(text()))
                    .padding(Padding::new(1, 1, 0, 0)),
            )
            .wrap(Wrap { trim: false });

        frame.render_widget(input, area);
    }

    /// The plan shown in the strip above the composer: the latest plan while
    /// it still has unfinished work. Completed plans leave the screen.
    fn plan_strip(&self) -> Option<&PlanView> {
        let plan = self.current_plan()?;
        if plan.items.is_empty()
            || plan
                .items
                .iter()
                .all(|item| item.status == PlanItemStatus::Done)
        {
            return None;
        }
        Some(plan)
    }

    fn plan_strip_height(&self, terminal_height: u16) -> u16 {
        // On short terminals the transcript and composer win.
        if terminal_height < 20 {
            return 0;
        }
        match self.plan_strip() {
            Some(plan) => (plan_strip_lines(plan).len() as u16).min(9),
            None => 0,
        }
    }

    fn draw_plan_strip(&self, frame: &mut Frame<'_>, area: Rect) {
        if area.height == 0 {
            return;
        }
        let Some(plan) = self.plan_strip() else {
            return;
        };
        let mut lines = plan_strip_lines(plan);
        lines.truncate(area.height as usize);
        frame.render_widget(
            Paragraph::new(lines).style(Style::default().bg(app_bg()).fg(text())),
            area,
        );
    }

    fn input_height(&self, terminal_height: u16) -> u16 {
        let attachment_lines = if self.pending_attachments.is_empty() {
            0
        } else {
            1 + COMPOSER_IMAGE_PREVIEW_HEIGHT
        };
        let text_lines = self.input.lines().count().max(1) as u16;
        let desired = (text_lines + attachment_lines + 2).clamp(3, 8);
        let max = terminal_height.saturating_sub(4).max(3);

        desired.min(max)
    }

    fn draw_status(&self, frame: &mut Frame<'_>, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Min(16),
                Constraint::Min(0),
                Constraint::Length(10),
                Constraint::Length(42),
            ])
            .split(area);
        let status = Paragraph::new(self.status_line_content())
            .alignment(Alignment::Left)
            .style(Style::default().bg(surface()));

        frame.render_widget(status, chunks[0]);
        self.draw_footer_hints(frame, chunks[1]);
        self.draw_context_gauge(frame, chunks[2]);
        self.draw_footer_telemetry(frame, chunks[3]);
    }

    fn context_usage_chars(&self) -> usize {
        transcript_char_usage(&self.transcript).total()
    }

    /// Snapshot for the /context modal: estimated tokens per category, the
    /// budget, and the compaction state. Uses the same ~4 chars/token
    /// estimate as the footer gauge and the core context engine.
    fn build_context_report(&self) -> ContextReport {
        let chars = transcript_char_usage(&self.transcript);
        // The header system messages conversation_history() prepends before
        // the transcript-derived messages (permission context, rolling
        // session state, optional plan directive).
        let header_len = 2 + usize::from(self.plan_mode);
        let system_tokens = self
            .conversation_history()
            .iter()
            .take(header_len)
            .map(medusa_core::context::message_tokens)
            .sum();
        let summary = self.context_engine.summary();

        ContextReport {
            instructions_tokens: medusa_core::context::baseline_instructions_tokens(
                self.tools.workspace(),
            ),
            system_tokens,
            message_tokens: chars.messages.div_ceil(4),
            tool_tokens: chars.tool_outputs.div_ceil(4),
            reasoning_tokens: chars.reasoning.div_ceil(4),
            plan_tokens: chars.plans.div_ceil(4),
            budget: medusa_core::context::context_max_tokens(),
            summary_covers: summary.as_ref().map(|summary| summary.covers),
            summary_tokens: summary
                .map(|summary| medusa_core::context::estimate_tokens(&summary.text))
                .unwrap_or(0),
        }
    }

    fn draw_context_gauge(&self, frame: &mut Frame<'_>, area: Rect) {
        if area.width < 4 {
            return;
        }
        // ~4 chars per token, matching medusa_core::context::estimate_tokens.
        let used = self.context_usage_chars().div_ceil(4);
        let max = medusa_core::context::context_max_tokens().max(1);
        let ratio = (used as f64 / max as f64).clamp(0.0, 1.0);
        let color = if ratio < 0.5 {
            palette().success
        } else if ratio < 0.8 {
            accent_color()
        } else {
            palette().error
        };
        let gauge = LineGauge::default()
            .filled_style(Style::default().fg(color).bg(surface()))
            .unfilled_style(Style::default().fg(palette().separator).bg(surface()))
            .ratio(ratio)
            .label(Span::styled(
                if area.width >= 8 {
                    format!("{:>3.0}%", ratio * 100.0)
                } else {
                    String::new()
                },
                muted(),
            ))
            .style(Style::default().bg(surface()));
        frame.render_widget(gauge, area);
    }

    fn draw_footer_telemetry(&self, frame: &mut Frame<'_>, area: Rect) {
        let running = self
            .background_jobs
            .values()
            .filter(|job| job.state == ToolRunState::Running)
            .count();
        let activity = self
            .turn_started_at
            .map(|t| format!("turn {}s", t.elapsed().as_secs()))
            .unwrap_or_else(|| self.scroll_footer_label());
        let tool_count = self
            .transcript
            .iter()
            .filter(|item| matches!(item, TranscriptItem::Tool(_)))
            .count();
        let text = format!(
            "tools {tool_count} · jobs {running} · {} · {activity}",
            format_token_count(self.session_usage.total())
        );
        let widget = Paragraph::new(Line::from(Span::styled(text, muted())))
            .alignment(Alignment::Left)
            .style(Style::default().bg(surface()));
        frame.render_widget(widget, area);
    }

    fn scroll_footer_label(&self) -> String {
        let Some(metrics) = self.current_chat_viewport_metrics() else {
            return "idle".to_string();
        };
        if metrics.max_scroll == 0 {
            return "idle".to_string();
        }
        if metrics.scroll == 0 {
            return "bottom".to_string();
        }
        format!("scroll {}%", scroll_progress_percent(&metrics))
    }

    fn status_line_content(&self) -> Line<'static> {
        let Some(toast) = &self.toast else {
            return Line::from(Span::styled(self.status_line.clone(), muted()));
        };

        Line::from(vec![
            Span::styled(
                toast_label(toast.kind),
                toast_style(toast.kind).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" · ", muted()),
            Span::styled(truncate(&toast.message, 96), value_style()),
        ])
    }

    fn draw_footer_hints(&self, frame: &mut Frame<'_>, area: Rect) {
        let hints = match self.focus() {
            UiFocus::Activity => "j/k · enter · x · esc",
            UiFocus::Transcript => "ctrl+end · pgup/dn",
            UiFocus::Modal => match self.active_modal {
                Some(Modal::Settings) => "↑/↓ · enter · esc",
                Some(Modal::Themes) => "↑/↓ · enter · esc",
                Some(Modal::Rewind) => "↑/↓ · enter · esc",
                Some(Modal::EditMessage) => "↑/↓ · enter · esc",
                Some(Modal::Models) => "↑/↓ · enter · esc",
                Some(Modal::Permissions) => "↑/↓ · enter · esc",
                Some(Modal::ImagePreview) => "j/k · +/- · d detach · o/y · esc",
                _ => "esc",
            },
            UiFocus::Composer if self.pending_decision().is_some() => {
                "j/k question · h/l option · 1-8 choose · enter send"
            }
            UiFocus::Composer => "enter · ctrl+p · ctrl+i/o/d",
        };
        let footer = Paragraph::new(Line::from(Span::styled(hints, muted())))
            .alignment(Alignment::Left)
            .style(Style::default().bg(surface()));
        frame.render_widget(footer, area);
    }

    fn input_title_content(&self) -> Line<'static> {
        if self.is_working() {
            return Line::from(light_sweep_spans(
                " ━━━━━━━ ",
                self.animation_tick,
                |style| style.bg(surface()),
            ));
        }

        if let Some(decision) = self.pending_decision() {
            let question = decision
                .questions
                .get(self.selected_decision_question_index())
                .map(|question| question.prompt.as_str())
                .unwrap_or(decision.title.as_str());
            return Line::from(vec![
                Span::styled(" Decision ", prompt_style().add_modifier(Modifier::BOLD)),
                Span::styled(
                    format!(" {} ", truncate(question, 48)),
                    accent().add_modifier(Modifier::BOLD),
                ),
            ]);
        }

        let mut spans = vec![
            Span::styled(" Message ", muted().add_modifier(Modifier::BOLD)),
            Span::styled(
                format!(" {} ", self.model.model_name()),
                accent().add_modifier(Modifier::BOLD),
            ),
        ];
        if self.plan_mode {
            spans.push(Span::styled(
                " plan ",
                Style::default()
                    .fg(palette().selected_fg)
                    .bg(palette().prompt)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        Line::from(spans)
    }

    /// Always-on-top approval prompt for the front of the queue. Drawn last
    /// so it overlays modals and streaming output alike.
    fn draw_approval_prompt(&self, frame: &mut Frame<'_>, area: Rect) {
        let Some(pending) = self.approval_queue.front() else {
            return;
        };
        let request = &pending.request;

        // Wrap width chosen to match the popup body so the full command shows
        // and a destructive tail can never hide past a truncation point.
        let wrap_width = area.width.saturating_sub(12).clamp(28, 72) as usize;
        let mut body: Vec<Line<'static>> = Vec::new();
        if let Some(command) = request.command.as_deref() {
            for (index, chunk) in wrap_str(command.trim(), wrap_width).into_iter().enumerate() {
                let prefix = if index == 0 { "  $ " } else { "    " };
                body.push(Line::from(vec![
                    Span::styled(prefix, prompt_style()),
                    Span::styled(chunk, value_style().add_modifier(Modifier::BOLD)),
                ]));
            }
            if request.background {
                body.push(Line::from(Span::styled(
                    "    runs as a background job",
                    muted(),
                )));
            }
            if request.sandbox_escalation {
                body.push(Line::from(Span::styled(
                    "    escapes the sandbox: writes outside the workspace and network allowed",
                    error_style(),
                )));
            }
        }
        for path in request.paths.iter().take(6) {
            body.push(Line::from(vec![
                Span::styled("  → ", separator_style()),
                Span::styled(truncate(path, wrap_width).to_string(), value_style()),
            ]));
        }
        if request.paths.len() > 6 {
            body.push(Line::from(Span::styled(
                format!("    … +{} more files", request.paths.len() - 6),
                muted(),
            )));
        }
        body.push(Line::from(""));
        let mut keys = vec![
            Span::styled("  y", success_style().add_modifier(Modifier::BOLD)),
            Span::styled(" allow once   ", muted()),
        ];
        // Escalations are one-shot by design: no always-allow.
        if !request.sandbox_escalation {
            keys.push(Span::styled(
                "a",
                prompt_style().add_modifier(Modifier::BOLD),
            ));
            keys.push(Span::styled(" always allow   ", muted()));
        }
        keys.extend([
            Span::styled("n", error_style().add_modifier(Modifier::BOLD)),
            Span::styled("/", muted()),
            Span::styled("esc", error_style().add_modifier(Modifier::BOLD)),
            Span::styled(" deny", muted()),
        ]);
        body.push(Line::from(keys));

        let height = (body.len() as u16)
            .saturating_add(2)
            .min(area.height.saturating_sub(2).max(4));
        let width = area
            .width
            .saturating_sub(8)
            .min(78)
            .min(area.width.saturating_sub(2))
            .max(area.width.min(30));
        let x = area.x + (area.width.saturating_sub(width)) / 2;
        let y = area
            .y
            .saturating_add(area.height.saturating_sub(height + 6));
        let popup = Rect {
            x,
            y,
            width,
            height,
        };

        frame.render_widget(Clear, popup);
        let queued = self.approval_queue.len();
        let heading = if request.sandbox_escalation {
            "Run unsandboxed?"
        } else {
            "Approval required"
        };
        let title = if queued > 1 {
            format!(" {heading} · {} (1/{queued}) ", request.tool.label())
        } else {
            format!(" {heading} · {} ", request.tool.label())
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(palette().prompt))
            .style(Style::default().bg(surface()).fg(text()))
            .title(title);
        let inner = popup.inner(Margin {
            horizontal: 1,
            vertical: 1,
        });
        frame.render_widget(block, popup);
        frame.render_widget(
            Paragraph::new(body).style(Style::default().bg(surface()).fg(text())),
            inner,
        );
    }

    fn draw_slash_suggestions(&self, frame: &mut Frame<'_>, shell_area: Rect) {
        let matches = self.slash_matches();
        if matches.is_empty() {
            return;
        }

        let area = command_palette_rect(shell_area, matches.len());
        frame.render_widget(Clear, area);
        let selected = self.slash_selection.min(matches.len().saturating_sub(1));
        let query = self.input.trim_start_matches('/');
        let inner = area.inner(Margin {
            horizontal: 2,
            vertical: 1,
        });
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .split(inner);
        let body = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(42), Constraint::Min(26)])
            .split(sections[1]);

        let visible_rows = body[0].height as usize;
        let offset = selected
            .saturating_add(1)
            .saturating_sub(visible_rows.max(1));
        let end = offset.saturating_add(visible_rows).min(matches.len());
        let items = matches[offset..end]
            .iter()
            .map(|(command, positions)| {
                let mut spans = highlighted_command_name_spans(command.name, positions, 11);
                spans.push(Span::styled(format!("{:<9}", command.category), muted()));
                spans.push(Span::styled(command.args, value_style()));
                ListItem::new(Line::from(spans))
            })
            .collect::<Vec<_>>();
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(accent_color()))
            .style(Style::default().bg(surface()).fg(text()))
            .title(" Command Palette ");
        frame.render_widget(block, area);

        let header = Paragraph::new(vec![
            Line::from(vec![
                Span::styled("MEDUSA", accent().add_modifier(Modifier::BOLD)),
                Span::styled(
                    " command surface",
                    value_style().add_modifier(Modifier::BOLD),
                ),
                Span::styled("  ", muted()),
                Span::styled(format!("{} matches", matches.len()), muted()),
            ]),
            Line::from(vec![
                Span::styled("query ", muted()),
                Span::styled(
                    if query.is_empty() {
                        "/".to_string()
                    } else {
                        format!("/{query}")
                    },
                    prompt_style(),
                ),
            ]),
        ])
        .style(Style::default().bg(surface()).fg(text()));
        frame.render_widget(header, sections[0]);

        let mut state = ListState::default().with_selected(selected.checked_sub(offset));
        let list = List::new(items)
            .style(Style::default().bg(surface()).fg(text()))
            .highlight_style(command_selected_style())
            .highlight_symbol("▌ ");

        frame.render_stateful_widget(list, body[0], &mut state);

        let divider_area = Rect::new(
            body[0].x.saturating_add(body[0].width),
            body[0].y,
            1,
            body[0].height,
        );
        let divider = Paragraph::new(
            (0..divider_area.height)
                .map(|_| Line::from(Span::styled("│", separator_style())))
                .collect::<Vec<_>>(),
        )
        .style(Style::default().bg(surface()));
        frame.render_widget(divider, divider_area);

        let command = matches[selected].0;
        let detail = Paragraph::new(command_palette_detail_lines(command))
            .style(Style::default().bg(surface()).fg(text()))
            .wrap(Wrap { trim: true });
        frame.render_widget(
            detail,
            body[1].inner(Margin {
                horizontal: 2,
                vertical: 0,
            }),
        );

        let footer = Paragraph::new(Line::from(vec![
            Span::styled("↑/↓ tab", prompt_style()),
            Span::styled(" select  ", muted()),
            Span::styled("pg", prompt_style()),
            Span::styled(" jump  ", muted()),
            Span::styled("enter", prompt_style()),
            Span::styled(" run  ", muted()),
            Span::styled("esc", prompt_style()),
            Span::styled(" close", muted()),
        ]))
        .alignment(Alignment::Right)
        .style(Style::default().bg(surface()));
        frame.render_widget(footer, sections[2]);
    }

    /// @file mention popup: same centered palette surface as the slash
    /// suggestions, but a single fuzzy-filtered list of workspace paths.
    fn draw_mention_suggestions(&self, frame: &mut Frame<'_>, shell_area: Rect) {
        if !self.mention_active() {
            return;
        }
        let matches = self.mention_matches();
        if matches.is_empty() {
            return;
        }

        let area = command_palette_rect(shell_area, matches.len());
        frame.render_widget(Clear, area);
        let selected = self.mention_selection.min(matches.len().saturating_sub(1));
        let query = self
            .active_mention_token()
            .map(|(_, _, query)| query)
            .unwrap_or_default();

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(accent_color()))
            .style(Style::default().bg(surface()).fg(text()))
            .title(" Files ");
        frame.render_widget(block, area);

        let inner = area.inner(Margin {
            horizontal: 2,
            vertical: 1,
        });
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .split(inner);

        let header = Paragraph::new(Line::from(vec![
            Span::styled("@", prompt_style()),
            Span::styled(query, prompt_style()),
            Span::styled("  ", muted()),
            Span::styled(format!("{} files", matches.len()), muted()),
        ]))
        .style(Style::default().bg(surface()).fg(text()));
        frame.render_widget(header, sections[0]);

        let visible_rows = sections[1].height as usize;
        let offset = selected
            .saturating_add(1)
            .saturating_sub(visible_rows.max(1));
        let end = offset.saturating_add(visible_rows).min(matches.len());
        let items = matches[offset..end]
            .iter()
            .map(|(path, positions)| ListItem::new(Line::from(mention_path_spans(path, positions))))
            .collect::<Vec<_>>();
        let mut state = ListState::default().with_selected(selected.checked_sub(offset));
        let list = List::new(items)
            .style(Style::default().bg(surface()).fg(text()))
            .highlight_style(command_selected_style())
            .highlight_symbol("▌ ");
        frame.render_stateful_widget(list, sections[1], &mut state);

        let footer = Paragraph::new(Line::from(vec![
            Span::styled("↑/↓", prompt_style()),
            Span::styled(" select  ", muted()),
            Span::styled("enter/tab", prompt_style()),
            Span::styled(" insert path  ", muted()),
            Span::styled("esc", prompt_style()),
            Span::styled(" close", muted()),
        ]))
        .alignment(Alignment::Right)
        .style(Style::default().bg(surface()));
        frame.render_widget(footer, sections[2]);
    }

    fn draw_modal(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let Some(modal) = self.active_modal else {
            return;
        };

        let popup_width = match modal {
            Modal::ImagePreview => area.width.saturating_sub(4).min(128),
            Modal::Settings | Modal::Themes => area.width.saturating_sub(8).min(94),
            Modal::Models | Modal::Permissions => area.width.saturating_sub(8).min(88),
            _ => area.width.saturating_sub(8).min(78),
        };
        let popup_height = area.height.saturating_sub(4).min(match modal {
            Modal::Commands => 18,
            Modal::Settings => 18,
            Modal::Help => 17,
            Modal::ImagePreview => 36,
            Modal::Workflows => 18,
            Modal::Jobs => 16,
            Modal::Sessions => 14,
            Modal::SessionTree => 18,
            Modal::Models | Modal::Reasoning | Modal::Permissions => 16,
            Modal::Themes => 18,
            Modal::Rewind => 16,
            Modal::EditMessage => 18,
            Modal::Mcp => 20,
            Modal::Agents => 18,
            Modal::Cost => 14,
            Modal::Context => 18,
        });
        let popup = centered_rect(area, popup_width, popup_height);
        frame.render_widget(Clear, popup);

        match modal {
            Modal::Commands => self.draw_commands_modal(frame, popup),
            Modal::Settings => self.draw_settings_modal(frame, popup),
            Modal::Help => self.draw_help_modal(frame, popup),
            Modal::ImagePreview => self.draw_image_preview_modal(frame, popup),
            Modal::Workflows => self.draw_workflows_modal(frame, popup),
            Modal::Jobs => self.draw_jobs_modal(frame, popup),
            Modal::Sessions => self.draw_sessions_modal(frame, popup),
            Modal::SessionTree => self.draw_session_tree_modal(frame, popup),
            Modal::Models => self.draw_models_modal(frame, popup),
            Modal::Reasoning => self.draw_reasoning_modal(frame, popup),
            Modal::Permissions => self.draw_permissions_modal(frame, popup),
            Modal::Themes => self.draw_themes_modal(frame, popup),
            Modal::Rewind => self.draw_rewind_modal(frame, popup),
            Modal::EditMessage => self.draw_edit_message_modal(frame, popup),
            Modal::Mcp => self.draw_mcp_modal(frame, popup),
            Modal::Agents => self.draw_agents_modal(frame, popup),
            Modal::Cost => self.draw_cost_modal(frame, popup),
            Modal::Context => self.draw_context_modal(frame, popup),
        }
    }

    /// `/agents`: named agents from .medusa/agents captured when the command
    /// ran. Esc/Enter closes (generic modal keys).
    fn draw_agents_modal(&self, frame: &mut Frame<'_>, area: Rect) {
        let mut lines: Vec<Line<'static>> = Vec::new();
        if self.agent_registry.is_empty() {
            lines.push(Line::from(Span::styled(
                "No named agents found in .medusa/agents.",
                muted(),
            )));
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "Define one agent per .md file in .medusa/agents:",
                value_style(),
            )));
            lines.push(Line::from(Span::styled(
                "  header lines name:, description:, tools: read|shell|edit|verify,",
                muted(),
            )));
            lines.push(Line::from(Span::styled(
                "  then a blank line, then the body used as the agent's system prompt.",
                muted(),
            )));
        }
        for agent in self.agent_registry.agents() {
            lines.push(Line::from(vec![
                Span::styled(
                    agent.name.clone(),
                    value_style().add_modifier(Modifier::BOLD),
                ),
                Span::styled("  · ", muted()),
                Span::styled(agent.tool_policy.label(), prompt_style()),
            ]));
            if !agent.description.is_empty() {
                lines.push(Line::from(Span::styled(
                    format!("  {}", truncate(&agent.description, 70)),
                    value_style(),
                )));
            }
            lines.push(Line::from(Span::styled(
                format!("  {}", truncate(&agent.path.display().to_string(), 70)),
                muted(),
            )));
            lines.push(Line::from(""));
        }
        for warning in self.agent_registry.warnings() {
            lines.push(Line::from(Span::styled(
                truncate(warning, 74),
                error_style(),
            )));
        }

        let paragraph = Paragraph::new(lines)
            .block(modal_block(" Named agents "))
            .style(Style::default().bg(surface()).fg(text()))
            .wrap(Wrap { trim: false });
        frame.render_widget(paragraph, area);
    }

    /// `/cost`: backend-reported token usage for the session and the most
    /// recent (or streaming) turn. Esc/Enter closes (generic modal keys).
    fn draw_cost_modal(&self, frame: &mut Frame<'_>, area: Rect) {
        let usage_line = |usage: TokenUsage| {
            Line::from(Span::styled(
                format!(
                    "  input {} · output {} · cached {}",
                    format_token_count(usage.input),
                    format_token_count(usage.output),
                    format_token_count(usage.cached)
                ),
                value_style(),
            ))
        };
        let request_count =
            |requests: usize| format!("{requests} request{}", if requests == 1 { "" } else { "s" });
        let (turn_label, turn_usage, turn_requests) = if self.is_working() {
            ("current turn", self.turn_usage, self.turn_requests)
        } else {
            ("last turn", self.last_turn_usage, self.last_turn_requests)
        };

        let mut lines = vec![
            Line::from(vec![
                Span::styled("session", value_style().add_modifier(Modifier::BOLD)),
                Span::styled(
                    format!(
                        "  · {} · {} total",
                        request_count(self.session_requests),
                        format_token_count(self.session_usage.total())
                    ),
                    muted(),
                ),
            ]),
            usage_line(self.session_usage),
            Line::from(""),
            Line::from(vec![
                Span::styled(turn_label, value_style().add_modifier(Modifier::BOLD)),
                Span::styled(
                    format!(
                        "  · {} · {} total",
                        request_count(turn_requests),
                        format_token_count(turn_usage.total())
                    ),
                    muted(),
                ),
            ]),
            usage_line(turn_usage),
            Line::from(""),
        ];
        if self.session_requests == 0 {
            lines.push(Line::from(Span::styled(
                "No usage reported yet — counts appear after the first model turn.",
                muted(),
            )));
            lines.push(Line::from(""));
        }
        lines.push(Line::from(Span::styled(
            "Counts are backend-reported tokens. What they cost depends on your plan and provider pricing; Medusa does not estimate dollar amounts.",
            muted(),
        )));

        let paragraph = Paragraph::new(lines)
            .block(modal_block(" Token usage "))
            .style(Style::default().bg(surface()).fg(text()))
            .wrap(Wrap { trim: false });
        frame.render_widget(paragraph, area);
    }

    /// `/context`: the estimated context picture captured when the command
    /// ran — per-category token estimates, the budget, and compaction state.
    /// Esc/Enter closes (generic modal keys).
    fn draw_context_modal(&self, frame: &mut Frame<'_>, area: Rect) {
        let Some(report) = self.context_report else {
            return;
        };

        let category_line = |label: &str, tokens: usize| {
            Line::from(vec![
                Span::styled(format!("  {label:<22}"), value_style()),
                Span::styled(format_token_count(tokens as u64), prompt_style()),
            ])
        };
        let percent = report.percent_used();
        let percent_style = if percent < 50 {
            success_style()
        } else if percent < 80 {
            prompt_style()
        } else {
            error_style()
        };

        let mut lines = vec![
            Line::from(vec![
                Span::styled(
                    "estimated usage",
                    value_style().add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(
                        "  · {} of {} budget · ",
                        format_token_count(report.total_tokens() as u64),
                        format_token_count(report.budget as u64)
                    ),
                    muted(),
                ),
                Span::styled(format!("{percent}%"), percent_style),
            ]),
            Line::from(""),
            category_line("system prompt (est)", report.instructions_tokens),
            category_line("session headers", report.system_tokens),
            category_line("messages", report.message_tokens),
            category_line("tool outputs", report.tool_tokens),
            category_line("reasoning", report.reasoning_tokens),
            category_line("plans & decisions", report.plan_tokens),
            Line::from(""),
        ];
        match report.summary_covers {
            Some(covers) => lines.push(Line::from(vec![
                Span::styled("compaction  ", value_style().add_modifier(Modifier::BOLD)),
                Span::styled(
                    format!(
                        "summary active · covers {covers} older messages · ~{}",
                        format_token_count(report.summary_tokens as u64)
                    ),
                    success_style(),
                ),
            ])),
            None => lines.push(Line::from(vec![
                Span::styled("compaction  ", value_style().add_modifier(Modifier::BOLD)),
                Span::styled("none — run /compact to fold older history early", muted()),
            ])),
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Estimates use ~4 characters per token; the backend's own count is what /cost reports.",
            muted(),
        )));

        let paragraph = Paragraph::new(lines)
            .block(modal_block(" Context "))
            .style(Style::default().bg(surface()).fg(text()))
            .wrap(Wrap { trim: false });
        frame.render_widget(paragraph, area);
    }

    /// `/mcp`: the snapshot captured when the command ran — servers, states,
    /// tools, and the config hint. Esc/Enter closes (generic modal keys).
    fn draw_mcp_modal(&self, frame: &mut Frame<'_>, area: Rect) {
        let mut lines: Vec<Line<'static>> = Vec::new();
        if self.mcp_statuses.is_empty() {
            lines.push(Line::from(Span::styled(
                "No MCP servers configured.",
                muted(),
            )));
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "Declare stdio servers in .medusa/mcp.json:",
                value_style(),
            )));
            lines.push(Line::from(Span::styled(
                r#"  {"servers": {"docs": {"command": "npx", "args": ["-y", "some-mcp-server"]}}}"#,
                muted(),
            )));
            lines.push(Line::from(Span::styled(
                r#"  Add "readOnly": true to allow a side-effect-free server in readonly mode."#,
                muted(),
            )));
        }
        for status in &self.mcp_statuses {
            let (state_text, state_style) = match &status.state {
                McpServerStateLabel::Idle => ("not started".to_string(), muted()),
                McpServerStateLabel::Connecting => ("connecting…".to_string(), prompt_style()),
                McpServerStateLabel::Ready => (
                    format!(
                        "ready · {} tool{}",
                        status.tools.len(),
                        if status.tools.len() == 1 { "" } else { "s" }
                    ),
                    success_style(),
                ),
                McpServerStateLabel::Disconnected => ("disconnected".to_string(), error_style()),
                McpServerStateLabel::Failed(error) => {
                    (format!("failed · {}", truncate(error, 64)), error_style())
                }
            };
            let mut header = vec![
                Span::styled(
                    status.name.clone(),
                    value_style().add_modifier(Modifier::BOLD),
                ),
                Span::styled("  ", muted()),
                Span::styled(state_text, state_style),
            ];
            if status.read_only {
                header.push(Span::styled("  · read-only", muted()));
            }
            if status.restarts > 0 {
                header.push(Span::styled(
                    format!("  · {} restart(s)", status.restarts),
                    muted(),
                ));
            }
            lines.push(Line::from(header));
            lines.push(Line::from(vec![
                Span::styled("  $ ", prompt_style()),
                Span::styled(truncate(&status.command_line, 68), muted()),
            ]));
            for tool in status.tools.iter().take(6) {
                lines.push(Line::from(Span::styled(
                    format!("    {tool}"),
                    value_style(),
                )));
            }
            if status.tools.len() > 6 {
                lines.push(Line::from(Span::styled(
                    format!("    … +{} more tools", status.tools.len() - 6),
                    muted(),
                )));
            }
            if let Some(tail) = &status.stderr_tail
                && matches!(
                    status.state,
                    McpServerStateLabel::Failed(_) | McpServerStateLabel::Disconnected
                )
            {
                let recent: Vec<&str> = tail.lines().rev().take(3).collect();
                for line in recent.into_iter().rev() {
                    lines.push(Line::from(Span::styled(
                        format!("    stderr: {}", truncate(line, 64)),
                        error_style(),
                    )));
                }
            }
            lines.push(Line::from(""));
        }
        lines.push(Line::from(vec![
            Span::styled("config ", muted()),
            Span::styled(".medusa/mcp.json", prompt_style()),
            Span::styled("  ·  ", muted()),
            Span::styled("/mcp restart <server>", prompt_style()),
            Span::styled(" reconnects a failed server", muted()),
        ]));

        let paragraph = Paragraph::new(lines)
            .block(modal_block(" MCP servers "))
            .style(Style::default().bg(surface()).fg(text()))
            .wrap(Wrap { trim: false });
        frame.render_widget(paragraph, area);
    }

    fn draw_image_preview_modal(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let attachments = self.image_attachments();
        let count = attachments.len();
        let selected = self.image_preview_index.min(count.saturating_sub(1));
        let title = if count == 0 {
            " Image Preview ".to_string()
        } else {
            format!(" Image Preview {}/{} ", selected + 1, count)
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(title)
            .border_style(Style::default().fg(accent_color()))
            .style(Style::default().bg(surface()).fg(text()));
        frame.render_widget(block, area);

        let inner = area.inner(Margin {
            horizontal: 2,
            vertical: 1,
        });
        if count == 0 || inner.height == 0 {
            let empty = Paragraph::new(vec![
                Line::from(Span::styled("No images attached yet.", muted())),
                Line::from(Span::styled(
                    "Paste one with Ctrl+I or drag an image path into the composer.",
                    value_style(),
                )),
            ])
            .style(Style::default().bg(surface()).fg(text()))
            .wrap(Wrap { trim: true });
            frame.render_widget(empty, inner);
            return;
        }

        let Some(attachment) = attachments.get(selected).cloned() else {
            return;
        };
        let image_input_warning = image_input_warning(self.model.provider_name());
        let header_height = if image_input_warning.is_some() { 3 } else { 2 };
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(header_height),
                Constraint::Min(4),
                Constraint::Length(1),
            ])
            .split(inner);
        let pending = self.current_preview_image_is_pending();
        let mut header_lines = vec![
            Line::from(vec![
                Span::styled(truncate(&attachment.name, 44), prompt_style()),
                Span::styled("  ", muted()),
                Span::styled(
                    format!("{}×{}", attachment.width, attachment.height),
                    value_style(),
                ),
                Span::styled("  ", muted()),
                Span::styled(human_bytes(attachment.size_bytes), muted()),
                Span::styled("  ", muted()),
                Span::styled(
                    if pending { "pending" } else { "sent" },
                    if pending { success_style() } else { muted() },
                ),
            ]),
            Line::from(vec![
                Span::styled(format!("zoom {}%", self.image_preview_zoom), accent()),
                Span::styled("  ", muted()),
                Span::styled(attachment.path.to_string_lossy().to_string(), muted()),
            ]),
        ];
        if let Some(warning) = image_input_warning {
            header_lines.push(Line::from(vec![
                Span::styled("preview only", prompt_style()),
                Span::styled(" · ", muted()),
                Span::styled(warning, muted()),
            ]));
        }
        let header = Paragraph::new(header_lines)
            .style(Style::default().bg(surface()).fg(text()))
            .wrap(Wrap { trim: true });
        frame.render_widget(header, sections[0]);

        let image_area = sections[1];
        let (preview_width, preview_height) =
            preview_image_dimensions(&attachment, image_area, self.image_preview_zoom);
        let x_offset = if preview_width < image_area.width {
            (image_area.width - preview_width) / 2
        } else {
            0
        };
        let y_offset = if preview_height < image_area.height {
            ((image_area.height - preview_height) / 2).min(i16::MAX as u16) as i16
        } else {
            0
        };
        let rendered = self.image_renderer.render(
            frame,
            &attachment,
            image_area,
            preview_width,
            preview_height,
            x_offset,
            y_offset,
        );
        if !rendered {
            let placeholder = Paragraph::new(image_placeholder_lines(
                &attachment,
                image_area.width.min(preview_width).max(10),
                image_area.height.min(preview_height).max(3),
            ))
            .alignment(Alignment::Center)
            .style(Style::default().bg(surface()).fg(text()))
            .wrap(Wrap { trim: false });
            frame.render_widget(placeholder, image_area);
        }

        let mut footer_spans = vec![
            Span::styled("j/k", prompt_style()),
            Span::styled(" image  ", muted()),
            Span::styled("+/-", prompt_style()),
            Span::styled(" zoom  ", muted()),
            Span::styled("0", prompt_style()),
            Span::styled(" reset  ", muted()),
        ];
        if pending {
            footer_spans.extend([
                Span::styled("d", prompt_style()),
                Span::styled(" detach  ", muted()),
            ]);
        }
        footer_spans.extend([
            Span::styled("o", prompt_style()),
            Span::styled(" open  ", muted()),
            Span::styled("y", prompt_style()),
            Span::styled(" copy path  ", muted()),
            Span::styled("esc", prompt_style()),
            Span::styled(" close", muted()),
        ]);
        let footer = Paragraph::new(Line::from(footer_spans))
            .alignment(Alignment::Right)
            .style(Style::default().bg(surface()));
        frame.render_widget(footer, sections[2]);
    }

    fn draw_commands_modal(&self, frame: &mut Frame<'_>, area: Rect) {
        let rows = SLASH_COMMANDS.iter().map(|command| {
            Row::new(vec![
                Cell::from(command.category).style(muted()),
                Cell::from(command.name).style(prompt_style()),
                Cell::from(command.args).style(muted()),
                Cell::from(command.description).style(value_style()),
            ])
        });
        let table = Table::new(
            rows,
            [
                Constraint::Length(10),
                Constraint::Length(12),
                Constraint::Length(14),
                Constraint::Min(20),
            ],
        )
        .header(
            Row::new(vec!["group", "command", "args", "description"])
                .style(muted().add_modifier(Modifier::BOLD)),
        )
        .block(modal_block(" Commands "))
        .column_spacing(1);

        frame.render_widget(table, area);
    }

    fn draw_settings_modal(&self, frame: &mut Frame<'_>, area: Rect) {
        let items = self.settings_items();
        let selected = self.settings_selection.min(items.len().saturating_sub(1));
        frame.render_widget(
            modal_block(" Settings ")
                .border_type(BorderType::Rounded)
                .padding(Padding::new(2, 2, 0, 0)),
            area,
        );

        let inner = area.inner(Margin {
            horizontal: 3,
            vertical: 1,
        });
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Min(6),
                Constraint::Length(1),
            ])
            .split(inner);
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(34), Constraint::Min(30)])
            .split(sections[1]);

        let header = Paragraph::new(vec![
            Line::from(vec![
                Span::styled("MEDUSA", accent().add_modifier(Modifier::BOLD)),
                Span::styled(" settings", value_style().add_modifier(Modifier::BOLD)),
            ]),
            Line::from(vec![
                Span::styled("command-surface controls", muted()),
                Span::styled(" · ", muted()),
                Span::styled("enter opens editable rows", prompt_style()),
            ]),
        ])
        .style(Style::default().bg(surface()).fg(text()));
        frame.render_widget(header, sections[0]);

        let rows = items
            .iter()
            .map(|item| {
                let marker = if item.editable { "● " } else { "· " };
                ListItem::new(Line::from(vec![
                    Span::styled(
                        marker,
                        if item.editable {
                            success_style()
                        } else {
                            muted()
                        },
                    ),
                    Span::styled(format!("{:<14}", item.key), value_style()),
                    Span::styled(truncate(&item.value, 15), muted()),
                ]))
            })
            .collect::<Vec<_>>();
        let mut state = ListState::default().with_selected(Some(selected));
        let list = List::new(rows)
            .style(Style::default().bg(surface()).fg(text()))
            .highlight_style(command_selected_style())
            .highlight_symbol("▌ ");
        frame.render_stateful_widget(list, columns[0], &mut state);

        if let Some(item) = items.get(selected) {
            let mut detail = vec![
                Line::from(vec![
                    Span::styled(item.key, prompt_style()),
                    Span::styled("  ", muted()),
                    Span::styled(&item.value, value_style().add_modifier(Modifier::BOLD)),
                ]),
                Line::from(""),
                Line::from(Span::styled(item.description, value_style())),
                Line::from(""),
                Line::from(vec![
                    Span::styled("action  ", muted()),
                    Span::styled(
                        item.action,
                        if item.editable {
                            prompt_style()
                        } else {
                            muted()
                        },
                    ),
                ]),
            ];
            if item.key == "theme" {
                detail.push(Line::from(""));
                detail.extend(theme_preview_lines(self.theme));
            } else if item.key == "model" {
                detail.push(Line::from(""));
                detail.extend(model_detail_lines(
                    self.model.model_name(),
                    self.model.model_name(),
                ));
            } else if item.key == "permissions" {
                detail.push(Line::from(""));
                detail.extend(permission_detail_lines(
                    self.permission_mode,
                    self.permission_mode,
                ));
            }

            let detail = Paragraph::new(detail)
                .style(Style::default().bg(surface()).fg(text()))
                .wrap(Wrap { trim: true });
            frame.render_widget(detail, columns[1]);
        }

        let footer = Paragraph::new(Line::from(vec![
            Span::styled("↑/↓ tab", prompt_style()),
            Span::styled(" select  ", muted()),
            Span::styled("enter", prompt_style()),
            Span::styled(" edit  ", muted()),
            Span::styled("esc", prompt_style()),
            Span::styled(" close", muted()),
        ]))
        .alignment(Alignment::Right)
        .style(Style::default().bg(surface()));
        frame.render_widget(footer, sections[2]);
    }

    fn draw_jobs_modal(&self, frame: &mut Frame<'_>, area: Rect) {
        let rows = self.background_jobs.values().rev().map(|job| {
            let elapsed = job
                .finished_at
                .unwrap_or_else(Instant::now)
                .saturating_duration_since(job.started_at);
            let status = match job.state {
                ToolRunState::Running => format!("running · {}s", elapsed.as_secs()),
                ToolRunState::Succeeded => format!(
                    "done · exit {} · {}s",
                    job.exit_code.unwrap_or(0),
                    elapsed.as_secs()
                ),
                ToolRunState::Failed => format!(
                    "failed · exit {} · {}s",
                    job.exit_code.unwrap_or(-1),
                    elapsed.as_secs()
                ),
            };
            Row::new(vec![
                Cell::from(job.id.clone()).style(prompt_style()),
                Cell::from(job.pid.to_string()).style(muted()),
                Cell::from(status).style(tool_output_style(job.state)),
                Cell::from(truncate(
                    &format!(
                        "{} · {}",
                        job.command,
                        abbreviate_home(&job.cwd.to_string_lossy())
                    ),
                    64,
                ))
                .style(value_style()),
            ])
        });
        let table = Table::new(
            rows,
            [
                Constraint::Length(16),
                Constraint::Length(8),
                Constraint::Length(22),
                Constraint::Min(24),
            ],
        )
        .header(
            Row::new(vec!["id", "pid", "status", "command"])
                .style(muted().add_modifier(Modifier::BOLD)),
        )
        .block(modal_block(" Background jobs · /kill /tail /restart "))
        .column_spacing(1);
        frame.render_widget(table, area);
    }

    fn draw_models_modal(&self, frame: &mut Frame<'_>, area: Rect) {
        frame.render_widget(
            modal_block(" Model ")
                .border_type(BorderType::Rounded)
                .padding(Padding::new(2, 2, 0, 0)),
            area,
        );
        let inner = area.inner(Margin {
            horizontal: 3,
            vertical: 1,
        });
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Min(5),
                Constraint::Length(1),
            ])
            .split(inner);
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(30), Constraint::Min(34)])
            .split(sections[1]);

        let header = Paragraph::new(vec![
            Line::from(vec![
                Span::styled("Model picker", accent().add_modifier(Modifier::BOLD)),
                Span::styled("  ", muted()),
                Span::styled(self.model.provider_name(), muted()),
                Span::styled("/", muted()),
                Span::styled(self.model.model_name().to_string(), prompt_style()),
            ]),
            Line::from(vec![
                Span::styled("enter saves for future turns", muted()),
                Span::styled(" · ", muted()),
                Span::styled("/model <id>", prompt_style()),
                Span::styled(" accepts any model id", muted()),
            ]),
        ])
        .style(Style::default().bg(surface()).fg(text()));
        frame.render_widget(header, sections[0]);

        let choices = model_choices(self.model.model_name());
        let rows = choices
            .iter()
            .map(|model| {
                let active = model == self.model.model_name();
                let (label, _) = model_display(model);
                let mut spans = vec![
                    Span::styled(
                        if active { "● " } else { "  " },
                        if active { success_style() } else { muted() },
                    ),
                    Span::styled(label.clone(), value_style()),
                ];
                // Show the wire slug beside a friendlier display name.
                if label != *model {
                    spans.push(Span::styled(format!("  {model}"), muted()));
                }
                ListItem::new(Line::from(spans))
            })
            .collect::<Vec<_>>();
        let mut state = ListState::default().with_selected(Some(self.model_selection));
        let list = List::new(rows)
            .style(Style::default().bg(surface()).fg(text()))
            .highlight_style(command_selected_style())
            .highlight_symbol("▌ ");
        frame.render_stateful_widget(list, columns[0], &mut state);

        let selected = choices
            .get(self.model_selection.min(choices.len().saturating_sub(1)))
            .map(String::as_str)
            .unwrap_or(self.model.model_name());
        let detail = Paragraph::new(model_detail_lines(selected, self.model.model_name()))
            .style(Style::default().bg(surface()).fg(text()))
            .wrap(Wrap { trim: true });
        frame.render_widget(detail, columns[1]);

        let footer = Paragraph::new(Line::from(vec![
            Span::styled("↑/↓ tab", prompt_style()),
            Span::styled(" choose  ", muted()),
            Span::styled("enter", prompt_style()),
            Span::styled(" save  ", muted()),
            Span::styled("esc", prompt_style()),
            Span::styled(" close", muted()),
        ]))
        .alignment(Alignment::Right)
        .style(Style::default().bg(surface()));
        frame.render_widget(footer, sections[2]);
    }

    fn draw_reasoning_modal(&self, frame: &mut Frame<'_>, area: Rect) {
        frame.render_widget(
            modal_block(" Reasoning effort ")
                .border_type(BorderType::Rounded)
                .padding(Padding::new(2, 2, 0, 0)),
            area,
        );
        let inner = area.inner(Margin {
            horizontal: 3,
            vertical: 1,
        });
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Min(5),
                Constraint::Length(1),
            ])
            .split(inner);
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(22), Constraint::Min(34)])
            .split(sections[1]);

        let model = self.model.model_name().to_string();
        let active = self.model.reasoning_effort().to_string();
        let (model_label, _) = model_display(&model);
        let header = Paragraph::new(vec![
            Line::from(vec![
                Span::styled("Reasoning effort", accent().add_modifier(Modifier::BOLD)),
                Span::styled("  for ", muted()),
                Span::styled(model_label, prompt_style()),
            ]),
            Line::from(Span::styled(
                "higher effort = deeper thinking, slower + more tokens",
                muted(),
            )),
        ])
        .style(Style::default().bg(surface()).fg(text()));
        frame.render_widget(header, sections[0]);

        let choices = reasoning_choices(&model, &active);
        let rows = choices
            .iter()
            .map(|effort| {
                let is_active = *effort == active;
                ListItem::new(Line::from(vec![
                    Span::styled(
                        if is_active { "● " } else { "  " },
                        if is_active { success_style() } else { muted() },
                    ),
                    Span::styled(effort.clone(), value_style()),
                ]))
            })
            .collect::<Vec<_>>();
        let mut state = ListState::default().with_selected(Some(self.reasoning_selection));
        let list = List::new(rows)
            .style(Style::default().bg(surface()).fg(text()))
            .highlight_style(command_selected_style())
            .highlight_symbol("▌ ");
        frame.render_stateful_widget(list, columns[0], &mut state);

        let selected = choices
            .get(
                self.reasoning_selection
                    .min(choices.len().saturating_sub(1)),
            )
            .map(String::as_str)
            .unwrap_or(active.as_str());
        let detail = Paragraph::new(reasoning_detail_lines(&model, selected, &active))
            .style(Style::default().bg(surface()).fg(text()))
            .wrap(Wrap { trim: true });
        frame.render_widget(detail, columns[1]);

        let footer = Paragraph::new(Line::from(vec![
            Span::styled("↑/↓ tab", prompt_style()),
            Span::styled(" choose  ", muted()),
            Span::styled("enter", prompt_style()),
            Span::styled(" save  ", muted()),
            Span::styled("esc", prompt_style()),
            Span::styled(" close", muted()),
        ]))
        .alignment(Alignment::Right)
        .style(Style::default().bg(surface()));
        frame.render_widget(footer, sections[2]);
    }

    fn draw_permissions_modal(&self, frame: &mut Frame<'_>, area: Rect) {
        frame.render_widget(
            modal_block(" Permissions ")
                .border_type(BorderType::Rounded)
                .padding(Padding::new(2, 2, 0, 0)),
            area,
        );
        let inner = area.inner(Margin {
            horizontal: 3,
            vertical: 1,
        });
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Min(5),
                Constraint::Length(1),
            ])
            .split(inner);
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(30), Constraint::Min(34)])
            .split(sections[1]);

        let header = Paragraph::new(vec![
            Line::from(vec![
                Span::styled("Permission mode", accent().add_modifier(Modifier::BOLD)),
                Span::styled("  ", muted()),
                Span::styled(self.permission_mode.label(), prompt_style()),
            ]),
            Line::from(vec![
                Span::styled("writes ", muted()),
                Span::styled(".medusa/permissions.json", value_style()),
                Span::styled(" and refreshes tools immediately", muted()),
            ]),
        ])
        .style(Style::default().bg(surface()).fg(text()));
        frame.render_widget(header, sections[0]);

        let rows = PermissionMode::all()
            .iter()
            .map(|mode| {
                let active = *mode == self.permission_mode;
                ListItem::new(Line::from(vec![
                    Span::styled(
                        if active { "● " } else { "  " },
                        if active { success_style() } else { muted() },
                    ),
                    Span::styled(format!("{:<12}", mode.label()), value_style()),
                    Span::styled(mode.name(), muted()),
                ]))
            })
            .collect::<Vec<_>>();
        let mut state = ListState::default().with_selected(Some(self.permission_selection));
        let list = List::new(rows)
            .style(Style::default().bg(surface()).fg(text()))
            .highlight_style(command_selected_style())
            .highlight_symbol("▌ ");
        frame.render_stateful_widget(list, columns[0], &mut state);

        let selected = PermissionMode::all()[self
            .permission_selection
            .min(PermissionMode::all().len().saturating_sub(1))];
        let detail = Paragraph::new(permission_detail_lines(selected, self.permission_mode))
            .style(Style::default().bg(surface()).fg(text()))
            .wrap(Wrap { trim: true });
        frame.render_widget(detail, columns[1]);

        let footer = Paragraph::new(Line::from(vec![
            Span::styled("↑/↓ tab", prompt_style()),
            Span::styled(" choose  ", muted()),
            Span::styled("enter", prompt_style()),
            Span::styled(" save  ", muted()),
            Span::styled("esc", prompt_style()),
            Span::styled(" close", muted()),
        ]))
        .alignment(Alignment::Right)
        .style(Style::default().bg(surface()));
        frame.render_widget(footer, sections[2]);
    }

    fn draw_themes_modal(&self, frame: &mut Frame<'_>, area: Rect) {
        frame.render_widget(
            modal_block(" Themes ")
                .border_type(BorderType::Rounded)
                .padding(Padding::new(2, 2, 0, 0)),
            area,
        );
        let inner = area.inner(Margin {
            horizontal: 3,
            vertical: 1,
        });
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Min(6),
                Constraint::Length(1),
            ])
            .split(inner);
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(30), Constraint::Min(34)])
            .split(sections[1]);

        let previewing = self
            .theme_preview_original
            .is_some_and(|original| original != self.theme);
        let header = Paragraph::new(vec![
            Line::from(vec![
                Span::styled("Theme picker", accent().add_modifier(Modifier::BOLD)),
                Span::styled("  ", muted()),
                Span::styled(
                    if previewing {
                        "live preview"
                    } else {
                        "choose theme"
                    },
                    if previewing { success_style() } else { muted() },
                ),
            ]),
            Line::from(vec![
                Span::styled("cycles repaint immediately", muted()),
                Span::styled(" · ", muted()),
                Span::styled("enter saves", prompt_style()),
                Span::styled(" · ", muted()),
                Span::styled("esc cancels", prompt_style()),
            ]),
        ])
        .style(Style::default().bg(surface()).fg(text()));
        frame.render_widget(header, sections[0]);

        let rows = ThemeKind::all()
            .iter()
            .map(|theme| {
                let state = if *theme == self.theme { "● " } else { "  " };
                ListItem::new(Line::from(vec![
                    Span::styled(
                        state,
                        if *theme == self.theme {
                            success_style()
                        } else {
                            muted()
                        },
                    ),
                    Span::styled(format!("{:<18}", theme.label()), value_style()),
                    Span::styled("██", Style::default().fg(theme.palette().accent)),
                ]))
            })
            .collect::<Vec<_>>();
        let mut state = ListState::default().with_selected(Some(self.theme_selection));
        let list = List::new(rows)
            .style(Style::default().bg(surface()).fg(text()))
            .highlight_style(command_selected_style())
            .highlight_symbol("▌ ");
        frame.render_stateful_widget(list, columns[0], &mut state);

        let selected_theme = ThemeKind::all()[self.theme_selection.min(ThemeKind::all().len() - 1)];
        let mut detail = vec![
            Line::from(vec![
                Span::styled(selected_theme.label(), prompt_style()),
                Span::styled("  ", muted()),
                Span::styled(selected_theme.name(), muted()),
            ]),
            Line::from(""),
            Line::from(Span::styled(selected_theme.description(), value_style())),
            Line::from(""),
        ];
        detail.extend(theme_preview_lines(selected_theme));
        detail.push(Line::from(""));
        detail.push(Line::from(vec![
            Span::styled("status  ", muted()),
            Span::styled(
                if previewing {
                    "previewing live; not saved yet"
                } else {
                    "saved"
                },
                if previewing {
                    prompt_style()
                } else {
                    success_style()
                },
            ),
        ]));
        let detail = Paragraph::new(detail)
            .style(Style::default().bg(surface()).fg(text()))
            .wrap(Wrap { trim: true });
        frame.render_widget(detail, columns[1]);

        let footer = Paragraph::new(Line::from(vec![
            Span::styled("↑/↓ tab", prompt_style()),
            Span::styled(" preview  ", muted()),
            Span::styled("enter", prompt_style()),
            Span::styled(" save  ", muted()),
            Span::styled("esc", prompt_style()),
            Span::styled(" cancel", muted()),
        ]))
        .alignment(Alignment::Right)
        .style(Style::default().bg(surface()));
        frame.render_widget(footer, sections[2]);
    }

    fn draw_help_modal(&self, frame: &mut Frame<'_>, area: Rect) {
        let help = Paragraph::new(vec![
            Line::from(vec![Span::styled("Slash commands", prompt_style())]),
            Line::from("Type / or press Ctrl+P to open the command palette."),
            Line::from("Use ↑/↓, PgUp/PgDn, Home/End, Enter, and Esc."),
            Line::from(""),
            Line::from(vec![Span::styled("Chat", prompt_style())]),
            Line::from("Enter sends. Shift+Enter inserts a newline."),
            Line::from("Ctrl+↑/↓ or mouse wheel scrolls chat."),
            Line::from("Ctrl+I attaches an image. Ctrl+O previews. Ctrl+D detaches latest."),
            Line::from(""),
            Line::from(vec![Span::styled("Themes", prompt_style())]),
            Line::from("Use /theme to browse themes or /theme opencode to switch directly."),
            Line::from(""),
            Line::from(vec![Span::styled("Modals", prompt_style())]),
            Line::from(
                "/plan or shift+tab toggles plan mode. Select a tool call with j/k, enter expands it.",
            ),
            Line::from("Esc or Enter closes simple popups."),
            Line::from(""),
            Line::from("Try /fork before risky work, or /tree to inspect branches."),
        ])
        .block(modal_block(" Help "))
        .wrap(Wrap { trim: false });
        frame.render_widget(help, area);
    }

    fn draw_workflows_modal(&self, frame: &mut Frame<'_>, area: Rect) {
        if self.workflows.is_empty() {
            let empty = Paragraph::new(vec![
                Line::from(Span::styled("No workflow runs yet.", muted())),
                Line::from(""),
                Line::from(vec![
                    Span::styled("Run ", muted()),
                    Span::styled("/workflow <task>", prompt_style()),
                    Span::styled(" for larger jobs that need subagents.", muted()),
                ]),
            ])
            .block(modal_block(" Workflows "))
            .wrap(Wrap { trim: true });
            frame.render_widget(empty, area);
            return;
        }

        let rows = self.workflows.iter().rev().take(12).map(|workflow| {
            let progress = workflow_progress(workflow);
            Row::new(vec![
                Cell::from(workflow_state_label(workflow.status))
                    .style(workflow_state_style(workflow.status)),
                Cell::from(truncate(&workflow.title, 34)).style(value_style()),
                Cell::from(workflow_progress_label(progress)).style(muted()),
                Cell::from(workflow_latest_activity(workflow))
                    .style(workflow_activity_style(workflow)),
            ])
        });

        let table = Table::new(
            rows,
            [
                Constraint::Length(10),
                Constraint::Min(26),
                Constraint::Length(24),
                Constraint::Min(26),
            ],
        )
        .header(
            Row::new(vec!["status", "workflow", "progress", "latest"])
                .style(muted().add_modifier(Modifier::BOLD)),
        )
        .block(modal_block(" Workflows "))
        .column_spacing(2);

        frame.render_widget(table, area);
    }

    fn draw_session_tree_modal(&self, frame: &mut Frame<'_>, area: Rect) {
        let entries = self
            .session
            .as_ref()
            .map(SessionStore::tree_entries)
            .unwrap_or_default();
        let rows = entries.into_iter().take(14).map(|entry| {
            let branch = if entry.depth == 0 { "●" } else { "└" };
            let indent = "  ".repeat(entry.depth);
            let name = format!("{indent}{branch} {}", entry.name);
            Row::new(vec![
                Cell::from(name).style(if entry.current {
                    prompt_style()
                } else {
                    value_style()
                }),
                Cell::from(entry.parent).style(muted()),
                Cell::from(entry.size).style(muted()),
                Cell::from(if entry.current { "yes" } else { "" }).style(prompt_style()),
            ])
        });
        let table = Table::new(
            rows,
            [
                Constraint::Min(28),
                Constraint::Length(18),
                Constraint::Length(10),
                Constraint::Length(8),
            ],
        )
        .header(
            Row::new(vec!["tree", "parent", "size", "current"])
                .style(muted().add_modifier(Modifier::BOLD)),
        )
        .block(modal_block(" Session Tree "))
        .column_spacing(2);
        frame.render_widget(table, area);
    }

    fn draw_sessions_modal(&self, frame: &mut Frame<'_>, area: Rect) {
        let sessions = self
            .session
            .as_ref()
            .map(SessionStore::list_sessions)
            .unwrap_or_default();
        let rows = sessions.into_iter().take(10).map(|session| {
            Row::new(vec![
                Cell::from(session.name).style(value_style()),
                Cell::from(session.parent).style(muted()),
                Cell::from(session.size).style(muted()),
                Cell::from(session.current).style(prompt_style()),
            ])
        });
        let table = Table::new(
            rows,
            [
                Constraint::Min(24),
                Constraint::Length(18),
                Constraint::Length(10),
                Constraint::Length(8),
            ],
        )
        .header(
            Row::new(vec!["session", "parent", "size", "current"])
                .style(muted().add_modifier(Modifier::BOLD)),
        )
        .block(modal_block(" Sessions "))
        .column_spacing(2);
        frame.render_widget(table, area);
    }

    fn draw_rewind_modal(&self, frame: &mut Frame<'_>, area: Rect) {
        match self.rewind_stage {
            RewindStage::Pick => self.draw_rewind_pick(frame, area),
            RewindStage::Confirm => self.draw_rewind_confirm(frame, area),
        }
    }

    fn draw_rewind_pick(&self, frame: &mut Frame<'_>, area: Rect) {
        frame.render_widget(modal_block(" Rewind "), area);
        let inner = area.inner(Margin {
            horizontal: 2,
            vertical: 1,
        });
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Min(4),
                Constraint::Length(1),
            ])
            .split(inner);

        let current_session = self
            .session
            .as_ref()
            .map(SessionStore::current_id)
            .unwrap_or_default();
        let header = Paragraph::new(vec![
            Line::from(Span::styled(
                "Restore files to the state before a turn",
                accent().add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                "Edit/patch tool changes only — shell-command changes are not rewound.",
                muted(),
            )),
        ])
        .style(Style::default().bg(surface()).fg(text()));
        frame.render_widget(header, sections[0]);

        let rows = self
            .rewind_entries
            .iter()
            .map(|entry| {
                let prompt = if entry.prompt_excerpt.is_empty() {
                    "(no prompt)".to_string()
                } else {
                    truncate(&entry.prompt_excerpt, 40)
                };
                let session_tag = if entry.session_id == current_session {
                    "this session".to_string()
                } else {
                    compact_session_id(&entry.session_id)
                };
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{:>8}", time_ago_ms(entry.created_at_ms)), muted()),
                    Span::styled("  ", muted()),
                    Span::styled(format!("{prompt:<43}"), value_style()),
                    Span::styled(
                        format!(
                            "{} file{}",
                            entry.files.len(),
                            if entry.files.len() == 1 { "" } else { "s" }
                        ),
                        prompt_style(),
                    ),
                    Span::styled("  ", muted()),
                    Span::styled(session_tag, muted()),
                ]))
            })
            .collect::<Vec<_>>();
        let mut state = ListState::default().with_selected(Some(self.rewind_selection));
        let list = List::new(rows)
            .style(Style::default().bg(surface()).fg(text()))
            .highlight_style(command_selected_style())
            .highlight_symbol("▌ ");
        frame.render_stateful_widget(list, sections[1], &mut state);

        let footer = Paragraph::new(Line::from(vec![
            Span::styled("↑/↓", prompt_style()),
            Span::styled(" select  ", muted()),
            Span::styled("enter", prompt_style()),
            Span::styled(" review  ", muted()),
            Span::styled("esc", prompt_style()),
            Span::styled(" close", muted()),
        ]))
        .alignment(Alignment::Right)
        .style(Style::default().bg(surface()));
        frame.render_widget(footer, sections[2]);
    }

    fn draw_rewind_confirm(&self, frame: &mut Frame<'_>, area: Rect) {
        frame.render_widget(modal_block(" Rewind · Confirm "), area);
        let inner = area.inner(Margin {
            horizontal: 2,
            vertical: 1,
        });
        let Some(entry) = self.selected_rewind_entry() else {
            return;
        };
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(6),
                Constraint::Min(2),
                Constraint::Length(1),
            ])
            .split(inner);

        let skipped_too_large = entry
            .files
            .iter()
            .filter(|file| {
                matches!(
                    file.pre,
                    medusa_core::checkpoint::PreImageKind::SkippedTooLarge
                        | medusa_core::checkpoint::PreImageKind::SkippedSymlink
                )
            })
            .count();
        let mut summary_lines = vec![
            Line::from(vec![
                Span::styled("Rewind to before: ", muted()),
                Span::styled(truncate(&entry.prompt_excerpt, 56), prompt_style()),
            ]),
            Line::from(vec![Span::styled(
                format!(
                    "{} · {} file{}",
                    time_ago_ms(entry.created_at_ms),
                    entry.files.len(),
                    if entry.files.len() == 1 { "" } else { "s" }
                ),
                value_style(),
            )]),
            Line::from(Span::styled(
                "Restores files changed by edit/patch tools in this and all newer turns.",
                muted(),
            )),
            Line::from(Span::styled(
                "Shell-command changes are NOT rewound and may leave mixed state.",
                error_preview_style(),
            )),
            Line::from(Span::styled(
                "A pre-rewind safety checkpoint is created first, so rewind is undoable.",
                muted(),
            )),
        ];
        if skipped_too_large > 0 {
            summary_lines.push(Line::from(Span::styled(
                format!("{skipped_too_large} file(s) could not be captured (too large or symlink) and will not be rewound."),
                error_preview_style(),
            )));
        }
        let summary = Paragraph::new(summary_lines)
            .style(Style::default().bg(surface()).fg(text()))
            .wrap(Wrap { trim: true });
        frame.render_widget(summary, sections[0]);

        let rows = self
            .rewind_confirm_options()
            .into_iter()
            .map(|option| ListItem::new(Line::from(Span::styled(option, value_style()))))
            .collect::<Vec<_>>();
        let mut state = ListState::default().with_selected(Some(self.rewind_confirm_selection));
        let list = List::new(rows)
            .style(Style::default().bg(surface()).fg(text()))
            .highlight_style(command_selected_style())
            .highlight_symbol("▌ ");
        frame.render_stateful_widget(list, sections[1], &mut state);

        let footer = Paragraph::new(Line::from(vec![
            Span::styled("↑/↓", prompt_style()),
            Span::styled(" choose  ", muted()),
            Span::styled("enter", prompt_style()),
            Span::styled(" confirm  ", muted()),
            Span::styled("esc", prompt_style()),
            Span::styled(" back", muted()),
        ]))
        .alignment(Alignment::Right)
        .style(Style::default().bg(surface()));
        frame.render_widget(footer, sections[2]);
    }

    /// `/edit`: pick a previous user message to backtrack to. The current
    /// timeline is forked into the session tree before truncation, so
    /// nothing is lost — /tree can revisit it.
    fn draw_edit_message_modal(&self, frame: &mut Frame<'_>, area: Rect) {
        frame.render_widget(modal_block(" Edit previous message "), area);
        let inner = area.inner(Margin {
            horizontal: 2,
            vertical: 1,
        });
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Min(4),
                Constraint::Length(1),
            ])
            .split(inner);

        let header = Paragraph::new(vec![
            Line::from(Span::styled(
                "Resend the conversation from an earlier message",
                accent().add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                "The current timeline is kept as a fork — browse it later with /tree.",
                muted(),
            )),
        ])
        .style(Style::default().bg(surface()).fg(text()));
        frame.render_widget(header, sections[0]);

        let rows = self
            .edit_picker_entries
            .iter()
            .enumerate()
            .map(|(ordinal, entry)| {
                let age = if ordinal == 0 {
                    "latest".to_string()
                } else {
                    format!("{} back", ordinal + 1)
                };
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{age:>8}"), muted()),
                    Span::styled("  ", muted()),
                    Span::styled(entry.preview.clone(), value_style()),
                ]))
            })
            .collect::<Vec<_>>();
        let mut state = ListState::default().with_selected(Some(self.edit_picker_selection));
        let list = List::new(rows)
            .style(Style::default().bg(surface()).fg(text()))
            .highlight_style(command_selected_style())
            .highlight_symbol("▌ ");
        frame.render_stateful_widget(list, sections[1], &mut state);

        let footer = Paragraph::new(Line::from(vec![
            Span::styled("↑/↓", prompt_style()),
            Span::styled(" select  ", muted()),
            Span::styled("enter", prompt_style()),
            Span::styled(" edit & resend  ", muted()),
            Span::styled("esc", prompt_style()),
            Span::styled(" close", muted()),
        ]))
        .alignment(Alignment::Right)
        .style(Style::default().bg(surface()));
        frame.render_widget(footer, sections[2]);
    }

    fn settings_rows(&self) -> Vec<(&'static str, String)> {
        self.settings_items()
            .into_iter()
            .map(|item| (item.key, item.value))
            .collect()
    }

    fn settings_items(&self) -> Vec<SettingsItem> {
        vec![
            SettingsItem {
                key: "model",
                value: self.model.model_name().to_string(),
                description: "The model Medusa sends coding turns to.",
                action: "enter opens model picker",
                editable: true,
            },
            SettingsItem {
                key: "theme",
                value: self.theme.name().to_string(),
                description: "Terminal color palette for Medusa surfaces, markdown, prompts, and tool activity.",
                action: "enter opens live theme picker",
                editable: true,
            },
            SettingsItem {
                key: "permissions",
                value: self.permission_mode.name().to_string(),
                description: "Preset controlling terminal commands and file mutation tools.",
                action: "enter opens permission mode picker",
                editable: true,
            },
            SettingsItem {
                key: "bell",
                value: if self.bell_setting { "on" } else { "off" }.to_string(),
                description: "Terminal bell when a long turn finishes or needs approval (MEDUSA_BELL=off overrides).",
                action: "enter toggles",
                editable: true,
            },
            SettingsItem {
                key: "workspace",
                value: self.cwd_display.clone(),
                description: "Project root used by the harness, tools, sessions, and workflows.",
                action: "launch Medusa from another directory to change it",
                editable: false,
            },
            SettingsItem {
                key: "git",
                value: if self.inside_git_repo {
                    "enabled"
                } else {
                    "not detected"
                }
                .to_string(),
                description: "Whether the current workspace has a Git repository.",
                action: "read only",
                editable: false,
            },
            SettingsItem {
                key: "session",
                value: if self.session.is_some() {
                    "enabled"
                } else {
                    "disabled"
                }
                .to_string(),
                description: "Session transcript persistence for resume and fork flows.",
                action: "/sessions or /tree",
                editable: false,
            },
            SettingsItem {
                key: "streaming",
                value: if self.is_working() { "active" } else { "idle" }.to_string(),
                description: "Current model stream state.",
                action: "read only",
                editable: false,
            },
            SettingsItem {
                key: "workflows",
                value: format!(
                    "{} total · {} active",
                    self.workflows.len(),
                    self.workflow_events.len()
                ),
                description: "Background workflow/subagent runs tracked by the TUI.",
                action: "/workflows",
                editable: false,
            },
            SettingsItem {
                key: "queued turns",
                value: self.queued_turns.len().to_string(),
                description: "User turns waiting behind active work.",
                action: "read only",
                editable: false,
            },
            SettingsItem {
                key: "background jobs",
                value: format!(
                    "{} total · {} running",
                    self.background_jobs.len(),
                    self.background_jobs
                        .values()
                        .filter(|job| job.state == ToolRunState::Running)
                        .count()
                ),
                description: "Detached terminal jobs started by Medusa.",
                action: "/jobs",
                editable: false,
            },
            SettingsItem {
                key: "uptime",
                value: format!("{}s", self.started_at.elapsed().as_secs()),
                description: "How long this TUI process has been open.",
                action: "read only",
                editable: false,
            },
            SettingsItem {
                key: "turn time",
                value: self
                    .turn_started_at
                    .map(|t| format!("{}s", t.elapsed().as_secs()))
                    .unwrap_or_else(|| "idle".to_string()),
                description: "Elapsed time for the active model turn.",
                action: "read only",
                editable: false,
            },
            SettingsItem {
                key: "scrollback",
                value: self.chat_scroll.to_string(),
                description: "Current transcript scroll offset from the bottom.",
                action: "ctrl+end returns to bottom",
                editable: false,
            },
        ]
    }

    fn kill_background_job(&mut self, id: &str) {
        let Some(job) = self.background_jobs.get(id) else {
            self.toast("Unknown background job", ToastKind::Error);
            self.status_line = "unknown job".to_string();
            return;
        };
        if job.state != ToolRunState::Running {
            self.toast("Job is not running", ToastKind::Warning);
            self.status_line = "job is not running".to_string();
            return;
        }
        let pid = job.pid;
        #[cfg(unix)]
        let status = Command::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .status();
        #[cfg(not(unix))]
        let status = Command::new("kill").arg(pid.to_string()).status();
        match status {
            Ok(status) if status.success() => {
                self.status_line = format!("kill sent · {id}");
                self.toast(self.status_line.clone(), ToastKind::Success);
            }
            Ok(status) => {
                self.status_line = format!("kill failed · exit {}", status.code().unwrap_or(-1));
                self.toast(self.status_line.clone(), ToastKind::Error);
            }
            Err(error) => {
                self.status_line = format!("kill failed: {error}");
                self.toast(self.status_line.clone(), ToastKind::Error);
            }
        }
    }

    fn tail_background_job(&mut self, id: &str) {
        let Some(job) = self.background_jobs.get(id) else {
            self.toast("Unknown background job", ToastKind::Error);
            return;
        };
        let text = if job.last_output.trim().is_empty() {
            format!(
                "job {id}\npid: {}\ncommand: {}\noutput: <not available yet>",
                job.pid, job.command
            )
        } else {
            format!(
                "job {id}\npid: {}\ncommand: {}\n\n{}",
                job.pid, job.command, job.last_output
            )
        };
        self.transcript
            .push(TranscriptItem::Message(ChatMessage::system(text)));
        self.touch_transcript();
        self.status_line = format!("tailed job {id}");
    }

    fn restart_background_job(&mut self, id: &str) {
        let Some(job) = self.background_jobs.get(id) else {
            self.toast("Unknown background job", ToastKind::Error);
            return;
        };
        let command = job.command.clone();
        self.start_exec_command(&command, true);
    }

    /// A runtime for tools the user invokes directly (/exec, /patch). The
    /// user typing the command IS the approval, so NeedsApproval auto-allows;
    /// hard denies still block. Uses an immediate closure (no channel) so it
    /// can run on the UI thread without deadlocking.
    fn user_tools(&self) -> ToolRuntime {
        self.tools
            .clone()
            .with_approval_handler(Arc::new(|_request| ApprovalDecision::AllowOnce))
    }

    fn start_exec_command(&mut self, command: &str, background: bool) {
        // A foreground /exec blocks the UI thread until the child exits, which
        // would also stall approval servicing for any running turn/workflow.
        if !background && (self.is_working() || self.has_active_workflows()) {
            self.status_line =
                "finish the current turn before running a foreground /exec".to_string();
            self.toast(
                "Busy — use /exec … & for background, or wait",
                ToastKind::Warning,
            );
            return;
        }
        self.push_tool_start("terminal.exec".to_string(), format!("$ {command}"));
        let request = TerminalExecRequest {
            command: command.to_string(),
            cwd: None,
            background,
            unsandboxed: false,
        };
        match self
            .user_tools()
            .with_background_events(self.background_job_sender.clone())
            .terminal_exec(request)
        {
            Ok(result) => {
                if result.background {
                    if let Some(id) = result.job_id.as_deref() {
                        self.attach_or_push_background_tool_start(id, command);
                        self.update_tool_result_by_id(
                            id,
                            ToolRunState::Running,
                            &terminal_result_output(&result),
                        );
                    } else {
                        self.push_tool_result("terminal.exec", terminal_result_output(&result));
                    }
                } else {
                    self.push_tool_result("terminal.exec", terminal_result_output(&result));
                }
                self.status_line = if result.background {
                    format!("terminal.exec background · pid {}", result.pid.unwrap_or(0))
                } else {
                    format!("terminal.exec exit {}", result.code.unwrap_or(-1))
                };
                self.toast(self.status_line.clone(), ToastKind::Success);
            }
            Err(error) => {
                self.push_tool_result("terminal.exec", format!("error: {error}"));
                self.status_line = "terminal.exec failed".to_string();
                self.toast("Command failed", ToastKind::Error);
            }
        }
    }
}

fn terminal_result_output(result: &TerminalExecResult) -> String {
    if result.background {
        return format!(
            "background: running\njob: {}\npid: {}\ncommand: {}",
            result.job_id.as_deref().unwrap_or("unknown"),
            result.pid.unwrap_or(0),
            result.command
        );
    }

    let mut output = format!("exit: {}\n", result.code.unwrap_or(-1));

    if result.stdout.is_empty() {
        output.push_str("stdout: <empty>\n");
    } else {
        output.push_str("stdout:\n");
        output.push_str(&result.stdout);
        if !result.stdout.ends_with('\n') {
            output.push('\n');
        }
    }

    if !result.stderr.is_empty() {
        output.push_str("stderr:\n");
        output.push_str(&result.stderr);
        if !result.stderr.ends_with('\n') {
            output.push('\n');
        }
    }

    output
}

fn parse_exec_command(raw: &str) -> (&str, bool) {
    let trimmed = raw.trim();
    for prefix in ["--background ", "--bg ", "-b "] {
        if let Some(command) = trimmed.strip_prefix(prefix) {
            return (command.trim(), true);
        }
    }
    (trimmed, false)
}

fn queue_count_suffix(count: usize) -> String {
    match count {
        0 | 1 => String::new(),
        count => format!(" · {count} waiting"),
    }
}

/// Character counts per transcript category, mirroring what each item
/// contributes to model context (messages, tool outputs, reasoning, and
/// plan/decision state).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct TranscriptCharUsage {
    messages: usize,
    tool_outputs: usize,
    reasoning: usize,
    plans: usize,
}

impl TranscriptCharUsage {
    fn total(&self) -> usize {
        self.messages + self.tool_outputs + self.reasoning + self.plans
    }
}

fn transcript_char_usage(transcript: &[TranscriptItem]) -> TranscriptCharUsage {
    let mut usage = TranscriptCharUsage::default();
    for item in transcript {
        match item {
            TranscriptItem::Message(msg) => usage.messages += msg.content.len(),
            // Tool results are function_call_outputs in model context; they
            // usually dominate usage, so count them too.
            TranscriptItem::Tool(run) => usage.tool_outputs += run.summary.len() + run.detail.len(),
            TranscriptItem::Reasoning(trace) => usage.reasoning += trace.content.len(),
            TranscriptItem::Plan(plan) => {
                usage.plans += plan.summary.len()
                    + plan
                        .items
                        .iter()
                        .map(|item| {
                            item.text.len() + item.evidence.iter().map(String::len).sum::<usize>()
                        })
                        .sum::<usize>();
            }
            TranscriptItem::Decision(decision) => {
                usage.plans += decision.title.len()
                    + decision.reason.len()
                    + decision.answer.as_ref().map_or(0, String::len)
                    + decision
                        .answers
                        .iter()
                        .map(|(key, value)| key.len() + value.len())
                        .sum::<usize>()
                    + decision
                        .questions
                        .iter()
                        .map(|question| {
                            question.prompt.len()
                                + question.options.iter().map(String::len).sum::<usize>()
                        })
                        .sum::<usize>();
            }
            TranscriptItem::Workflow(_) => {}
        }
    }
    usage
}

/// Estimated context composition captured when /context ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ContextReport {
    instructions_tokens: usize,
    system_tokens: usize,
    message_tokens: usize,
    tool_tokens: usize,
    reasoning_tokens: usize,
    plan_tokens: usize,
    budget: usize,
    summary_covers: Option<usize>,
    summary_tokens: usize,
}

impl ContextReport {
    fn total_tokens(&self) -> usize {
        self.instructions_tokens
            + self.system_tokens
            + self.message_tokens
            + self.tool_tokens
            + self.reasoning_tokens
            + self.plan_tokens
    }

    fn percent_used(&self) -> usize {
        self.total_tokens() * 100 / self.budget.max(1)
    }
}

/// Compact token count for footers and toasts: "812 tok", "1.23k tok",
/// "2.05M tok".
fn format_token_count(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.2}M tok", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.2}k tok", tokens as f64 / 1_000.0)
    } else {
        format!("{tokens} tok")
    }
}

#[derive(Debug, Clone, Copy)]
struct SlashCommand {
    name: &'static str,
    args: &'static str,
    category: &'static str,
    description: &'static str,
}

const SLASH_COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: "/help",
        args: "",
        category: "system",
        description: "Show available slash commands",
    },
    SlashCommand {
        name: "/settings",
        args: "",
        category: "system",
        description: "Show current Medusa settings",
    },
    SlashCommand {
        name: "/model",
        args: "<name>",
        category: "model",
        description: "Change the model used for new turns",
    },
    SlashCommand {
        name: "/reasoning",
        args: "[effort]",
        category: "model",
        description: "Set the reasoning/thinking effort (low…xhigh, model-specific)",
    },
    SlashCommand {
        name: "/permissions",
        args: "<mode>",
        category: "system",
        description: "Change terminal and file mutation permissions",
    },
    SlashCommand {
        name: "/reload",
        args: "",
        category: "system",
        description: "Restart Medusa and continue the current session",
    },
    SlashCommand {
        name: "/plan",
        args: "",
        category: "agent",
        description: "Toggle plan mode: explore read-only and propose a plan before editing",
    },
    SlashCommand {
        name: "/workflows",
        args: "",
        category: "view",
        description: "Show workflow runs and subagent progress",
    },
    SlashCommand {
        name: "/workflow",
        args: "<script|task> [args]",
        category: "agent",
        description: "Run a saved JS workflow script or the built-in subagent pipeline",
    },
    SlashCommand {
        name: "/sessions",
        args: "",
        category: "session",
        description: "Browse workspace session files",
    },
    SlashCommand {
        name: "/tree",
        args: "",
        category: "session",
        description: "Show workspace session branches",
    },
    SlashCommand {
        name: "/resume",
        args: "<session>",
        category: "session",
        description: "Switch to a saved workspace session",
    },
    SlashCommand {
        name: "/fork",
        args: "",
        category: "session",
        description: "Fork the current session before risky work",
    },
    SlashCommand {
        name: "/rewind",
        args: "",
        category: "session",
        description: "Restore files to the state before a previous turn",
    },
    SlashCommand {
        name: "/edit",
        args: "",
        category: "session",
        description: "Edit a previous message and resend from there (forks the timeline)",
    },
    SlashCommand {
        name: "/review",
        args: "",
        category: "tools",
        description: "Seed the composer with a code-review prompt for pending changes",
    },
    SlashCommand {
        name: "/clear",
        args: "",
        category: "session",
        description: "Clear the current transcript",
    },
    SlashCommand {
        name: "/cost",
        args: "",
        category: "context",
        description: "Show session and last-turn token usage",
    },
    SlashCommand {
        name: "/context",
        args: "",
        category: "context",
        description: "Show estimated context usage against the token budget",
    },
    SlashCommand {
        name: "/compact",
        args: "",
        category: "context",
        description: "Summarize older history now to free context",
    },
    SlashCommand {
        name: "/theme",
        args: "<name>",
        category: "theme",
        description: "Switch UI theme",
    },
    SlashCommand {
        name: "/tools",
        args: "",
        category: "tools",
        description: "List model-accessible local tools",
    },
    SlashCommand {
        name: "/skills",
        args: "",
        category: "context",
        description: "List workspace skills",
    },
    SlashCommand {
        name: "/agents",
        args: "",
        category: "context",
        description: "List named agents defined in .medusa/agents",
    },
    SlashCommand {
        name: "/mcp",
        args: "[restart <server>]",
        category: "tools",
        description: "List MCP servers and their tools, or restart one",
    },
    SlashCommand {
        name: "/auth",
        args: "",
        category: "system",
        description: "Check Codex/ChatGPT auth status",
    },
    SlashCommand {
        name: "/jobs",
        args: "",
        category: "tools",
        description: "Show background shell jobs",
    },
    SlashCommand {
        name: "/kill",
        args: "<job-id>",
        category: "tools",
        description: "Stop a running background job",
    },
    SlashCommand {
        name: "/tail",
        args: "<job-id>",
        category: "tools",
        description: "Show captured output for a background job",
    },
    SlashCommand {
        name: "/restart",
        args: "<job-id>",
        category: "tools",
        description: "Restart a background job command",
    },
    SlashCommand {
        name: "/exec",
        args: "<command>",
        category: "tools",
        description: "Run a shell command in this workspace",
    },
    SlashCommand {
        name: "/patch",
        args: "<path>",
        category: "tools",
        description: "Apply a unified diff file with git apply",
    },
];

const THEME_SLASH_COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: "/theme next",
        args: "",
        category: "theme",
        description: "Switch to the next UI theme",
    },
    SlashCommand {
        name: "/theme prev",
        args: "",
        category: "theme",
        description: "Switch to the previous UI theme",
    },
    SlashCommand {
        name: "/theme medusa",
        args: "",
        category: "theme",
        description: "Sharp black, acid green, warm prompt accents",
    },
    SlashCommand {
        name: "/theme opencode",
        args: "",
        category: "theme",
        description: "Quiet blue command surface with crisp contrast",
    },
    SlashCommand {
        name: "/theme tokyonight",
        args: "",
        category: "theme",
        description: "Deep navy with cyan highlights",
    },
    SlashCommand {
        name: "/theme catppuccin",
        args: "",
        category: "theme",
        description: "Soft mocha surface with rosewater accents",
    },
    SlashCommand {
        name: "/theme dracula",
        args: "",
        category: "theme",
        description: "Inky violet with neon pink and green highlights",
    },
    SlashCommand {
        name: "/theme nord",
        args: "",
        category: "theme",
        description: "Arctic blue-gray calm with frosty cyan accents",
    },
    SlashCommand {
        name: "/theme gruvbox",
        args: "",
        category: "theme",
        description: "Retro warm earth tones with punchy orange prompts",
    },
    SlashCommand {
        name: "/theme solarized-dark",
        args: "",
        category: "theme",
        description: "Low-glare teal base with balanced amber accents",
    },
    SlashCommand {
        name: "/theme material-dark",
        args: "",
        category: "theme",
        description: "Blue-grey Material base with balanced teal and amber",
    },
    SlashCommand {
        name: "/theme material-teal",
        args: "",
        category: "theme",
        description: "Material teal command surface with cyan tool accents",
    },
    SlashCommand {
        name: "/theme material-amber",
        args: "",
        category: "theme",
        description: "Material amber selection with teal prompts",
    },
    SlashCommand {
        name: "/theme material-indigo",
        args: "",
        category: "theme",
        description: "Material indigo focus with light-blue tooling",
    },
    SlashCommand {
        name: "/theme material-rose",
        args: "",
        category: "theme",
        description: "Material rose accents with teal supporting signals",
    },
    SlashCommand {
        name: "/theme rose-pine",
        args: "",
        category: "theme",
        description: "Muted rose and gold over a soho-night violet base",
    },
    SlashCommand {
        name: "/theme ayu-mirage",
        args: "",
        category: "theme",
        description: "Dusky slate with warm orange and sky-blue accents",
    },
    SlashCommand {
        name: "/theme everforest",
        args: "",
        category: "theme",
        description: "Soft forest greens with warm bark and sage tones",
    },
    SlashCommand {
        name: "/theme vesper",
        args: "",
        category: "theme",
        description: "Near-black minimalism with a single peach accent",
    },
];

/// Score a command against the palette query. Lower scores rank higher. The
/// returned positions are byte offsets of matched characters inside the
/// command name without its leading slash, used for match highlighting.
fn slash_match(command: &SlashCommand, query: &str) -> Option<(u8, Vec<usize>)> {
    if query.is_empty() {
        return Some((10, Vec::new()));
    }

    let name = command.name.trim_start_matches('/').to_ascii_lowercase();
    if name == query {
        return Some((0, (0..name.len()).collect()));
    }
    if let Some(start) = name.find(query) {
        let score = if start == 0 { 1 } else { 2 };
        return Some((score, (start..start + query.len()).collect()));
    }
    if let Some(positions) = subsequence_positions(&name, query) {
        return Some((3, positions));
    }

    let category = command.category.to_ascii_lowercase();
    let description = command.description.to_ascii_lowercase();
    let args = command.args.to_ascii_lowercase();
    if category.starts_with(query) {
        Some((4, Vec::new()))
    } else if category.contains(query) {
        Some((5, Vec::new()))
    } else if description.contains(query) {
        Some((6, Vec::new()))
    } else if args.contains(query) {
        Some((7, Vec::new()))
    } else {
        None
    }
}

/// Score a workspace path against an @mention query (already lowercased).
/// Lower scores rank first: 0 file-name prefix, 1 any path-segment prefix,
/// 2 substring, 3 subsequence. Matched byte positions index the full path.
fn mention_match(path: &str, query: &str) -> Option<(u8, Vec<usize>)> {
    if query.is_empty() {
        return Some((4, Vec::new()));
    }

    let lower = path.to_ascii_lowercase();
    let name_start = lower.rfind('/').map(|index| index + 1).unwrap_or(0);
    if lower[name_start..].starts_with(query) {
        return Some((0, (name_start..name_start + query.len()).collect()));
    }
    for (index, _) in lower.match_indices('/') {
        let segment_start = index + 1;
        if lower[segment_start..].starts_with(query) {
            return Some((1, (segment_start..segment_start + query.len()).collect()));
        }
    }
    if lower.starts_with(query) {
        return Some((1, (0..query.len()).collect()));
    }
    if let Some(start) = lower.find(query) {
        return Some((2, (start..start + query.len()).collect()));
    }
    subsequence_positions(&lower, query).map(|positions| (3, positions))
}

/// Breadth-first workspace file walk for the mention picker: relative paths
/// with '/' separators, junk directories skipped, capped at `max` entries.
/// Breadth-first order means shallow files survive the cap in big trees.
fn collect_workspace_files(root: &Path, max: usize) -> Vec<String> {
    let mut files = Vec::new();
    let mut queue = VecDeque::from([root.to_path_buf()]);
    while let Some(dir) = queue.pop_front() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        let mut entries = entries.flatten().collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            if files.len() >= max {
                return files;
            }
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let name = entry.file_name().to_string_lossy().to_string();
            if file_type.is_dir() {
                if !MENTION_SKIP_DIRS.contains(&name.as_str()) {
                    queue.push_back(entry.path());
                }
            } else if file_type.is_file()
                && name != ".DS_Store"
                && let Ok(relative) = entry.path().strip_prefix(root)
            {
                files.push(relative.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    files
}

/// Append `- <note>` under the `## Notes` section of AGENTS.md at the
/// workspace root, creating the file or the section when missing. The model
/// sees the note automatically on the next turn: project instructions are
/// reloaded from AGENTS.md at the start of every turn.
fn append_quick_memory(workspace: &Path, note: &str) -> Result<()> {
    let path = workspace.join("AGENTS.md");
    let existing = if path.exists() {
        fs::read_to_string(&path).wrap_err_with(|| format!("failed to read {}", path.display()))?
    } else {
        String::new()
    };
    let updated = quick_memory_content(&existing, note);
    fs::write(&path, updated).wrap_err_with(|| format!("failed to write {}", path.display()))
}

/// Collapse a quick-memory note to a single safe line so it can never forge a
/// new AGENTS.md section. Embedded newlines (and the whitespace around them)
/// fold into single spaces, and a leading `#` run is stripped — a multi-line
/// paste like `deploy\n## Deploy\nrun` becomes one bullet, and the insert-point
/// scan (which breaks on any line starting with `#`) always steps over it.
fn sanitize_quick_memory_note(note: &str) -> String {
    let collapsed = note
        .split(['\n', '\r'])
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    collapsed.trim_start_matches('#').trim_start().to_string()
}

/// Pure content transform behind [`append_quick_memory`]: the note lands at
/// the end of the `## Notes` section (before any following heading),
/// creating the file skeleton or the section when absent.
fn quick_memory_content(existing: &str, note: &str) -> String {
    let note = sanitize_quick_memory_note(note);
    let bullet = format!("- {note}");
    if existing.trim().is_empty() {
        return format!("{QUICK_MEMORY_HEADER}\n\n{QUICK_MEMORY_SECTION}\n\n{bullet}\n");
    }

    let mut lines = existing.lines().map(str::to_string).collect::<Vec<_>>();
    let Some(section) = lines
        .iter()
        .position(|line| line.trim() == QUICK_MEMORY_SECTION)
    else {
        let mut result = existing.trim_end().to_string();
        result.push_str(&format!("\n\n{QUICK_MEMORY_SECTION}\n\n{bullet}\n"));
        return result;
    };

    let mut insert_at = lines.len();
    for (index, line) in lines.iter().enumerate().skip(section + 1) {
        if line.trim_start().starts_with('#') {
            insert_at = index;
            break;
        }
    }
    while insert_at > section + 1 && lines[insert_at - 1].trim().is_empty() {
        insert_at -= 1;
    }
    lines.insert(insert_at, bullet);
    let mut result = lines.join("\n");
    result.push('\n');
    result
}

/// Fuzzy subsequence match: every query character appears in order in the
/// name (so "wf" matches "workflow"). Returns matched byte positions.
fn subsequence_positions(name: &str, query: &str) -> Option<Vec<usize>> {
    if query.chars().count() < 2 {
        return None;
    }
    let mut positions = Vec::new();
    let mut name_chars = name.char_indices();
    for query_char in query.chars() {
        loop {
            let (index, name_char) = name_chars.next()?;
            if name_char == query_char {
                positions.push(index);
                break;
            }
        }
    }
    Some(positions)
}

/// Render a workspace path with fuzzy-matched bytes highlighted.
fn mention_path_spans(path: &str, positions: &[usize]) -> Vec<Span<'static>> {
    path.char_indices()
        .map(|(index, ch)| {
            let style = if positions.contains(&index) {
                accent().add_modifier(Modifier::BOLD)
            } else {
                value_style()
            };
            Span::styled(ch.to_string(), style)
        })
        .collect()
}

/// Render a command name with matched characters highlighted, padded to
/// `width` display columns.
fn highlighted_command_name_spans(
    name: &'static str,
    positions: &[usize],
    width: usize,
) -> Vec<Span<'static>> {
    let body = name.trim_start_matches('/');
    let mut spans = vec![Span::styled("/", prompt_style())];
    for (index, ch) in body.char_indices() {
        let style = if positions.contains(&index) {
            accent().add_modifier(Modifier::BOLD)
        } else {
            prompt_style()
        };
        spans.push(Span::styled(ch.to_string(), style));
    }
    let used = 1 + body.chars().count();
    if used < width {
        spans.push(Span::raw(" ".repeat(width - used)));
    }
    spans
}

fn command_palette_detail_lines(command: &SlashCommand) -> Vec<Line<'static>> {
    let signature = if command.args.is_empty() {
        command.name.to_string()
    } else {
        format!("{} {}", command.name, command.args)
    };
    let action = if command.args.is_empty() {
        "Enter runs immediately."
    } else {
        "Enter places the command in the composer."
    };

    vec![
        Line::from(vec![
            Span::styled(command.category, muted().add_modifier(Modifier::BOLD)),
            Span::styled("  ", muted()),
            Span::styled(signature, prompt_style()),
        ]),
        Line::from(""),
        Line::from(Span::styled(command.description, value_style())),
        Line::from(""),
        Line::from(vec![
            Span::styled("action  ", muted()),
            Span::styled(action, value_style()),
        ]),
        Line::from(vec![
            Span::styled("scope   ", muted()),
            Span::styled(command.category, muted()),
        ]),
    ]
}

fn tools_text() -> String {
    [
        "tools",
        "explore.batch  Run parallel read-only probes and return evidence",
        "file.read      Read files by path/range",
        "file.search    Search file contents by regex",
        "file.glob      Find files by name pattern",
        "fs.list        List workspace paths",
        "file.edit      Replace exact old/new strings",
        "file.patch     Apply Codex patches or git diffs",
        "terminal.exec  Run shell commands/tests/builds",
        "task.update    Update current status",
        "plan.update    Replace visible task checklist",
        "decision.request Queue planning questions for the user",
    ]
    .join("\n")
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct PlanProgress {
    pending: usize,
    active: usize,
    done: usize,
    blocked: usize,
}

fn plan_progress(plan: &PlanView) -> PlanProgress {
    let mut progress = PlanProgress::default();
    for item in &plan.items {
        match item.status {
            PlanItemStatus::Pending => progress.pending += 1,
            PlanItemStatus::Active => progress.active += 1,
            PlanItemStatus::Done => progress.done += 1,
            PlanItemStatus::Blocked => progress.blocked += 1,
        }
    }
    progress
}

/// Items shown in the plan strip before folding the tail behind "+N more".
const PLAN_STRIP_MAX_ITEMS: usize = 6;

fn plan_strip_lines(plan: &PlanView) -> Vec<Line<'static>> {
    let progress = plan_progress(plan);
    let mut lines = Vec::new();

    let mut header = vec![
        Span::styled("plan", tool_label_style().add_modifier(Modifier::BOLD)),
        Span::styled(" · ", muted()),
        Span::styled(
            format!("{}/{}", progress.done, plan.items.len()),
            success_style(),
        ),
    ];
    if progress.blocked > 0 {
        header.extend([
            Span::styled(" · ", muted()),
            Span::styled(format!("{} blocked", progress.blocked), error_style()),
        ]);
    }
    if !plan.summary.trim().is_empty() {
        header.extend([
            Span::styled(" · ", muted()),
            Span::styled(truncate(&plan.summary, 72), muted()),
        ]);
    }
    lines.push(Line::from(header));

    // Long plans fold the completed prefix into one line so the strip always
    // centers on what's happening now.
    let mut start = 0;
    if plan.items.len() > PLAN_STRIP_MAX_ITEMS {
        let leading_done = plan
            .items
            .iter()
            .take_while(|item| item.status == PlanItemStatus::Done)
            .count();
        if leading_done > 1 {
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled("✓", success_style()),
                Span::styled(format!(" {leading_done} done"), success_style()),
            ]));
            start = leading_done;
        }
    }

    let remaining = &plan.items[start..];
    let shown = remaining.len().min(PLAN_STRIP_MAX_ITEMS);
    for item in remaining.iter().take(shown) {
        lines.push(Line::from(vec![
            Span::raw("  "),
            plan_status_marker_span(item.status),
            Span::raw(" "),
            Span::styled(truncate(&item.text, 110), plan_status_style(item.status)),
        ]));
    }
    if remaining.len() > shown {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("… +{} more", remaining.len() - shown), muted()),
        ]));
    }

    lines
}

fn plan_status_marker_span(status: PlanItemStatus) -> Span<'static> {
    match status {
        PlanItemStatus::Pending => Span::styled("·", muted()),
        PlanItemStatus::Active => Span::styled("●", prompt_style()),
        PlanItemStatus::Done => Span::styled("✓", success_style()),
        PlanItemStatus::Blocked => Span::styled("×", error_style()),
    }
}

fn plan_status_style(status: PlanItemStatus) -> Style {
    match status {
        PlanItemStatus::Pending => muted(),
        PlanItemStatus::Active => prompt_style(),
        PlanItemStatus::Done => success_style(),
        PlanItemStatus::Blocked => error_style(),
    }
}

fn append_decision_rows(
    rows: &mut Vec<TranscriptRow>,
    decision: &DecisionView,
    selected_question: usize,
) {
    let state = if decision.answered {
        "answered"
    } else {
        "waiting"
    };
    let mut header = vec![
        Span::styled("decision", prompt_style().add_modifier(Modifier::BOLD)),
        Span::styled(" · ", muted()),
        Span::styled(
            format!("{} question(s)", decision.questions.len()),
            value_style(),
        ),
        Span::styled(" · ", muted()),
        Span::styled(
            state,
            if decision.answered {
                success_style()
            } else {
                prompt_style()
            },
        ),
    ];
    if !decision.title.trim().is_empty() {
        header.extend([
            Span::styled(" · ", muted()),
            Span::styled(truncate(&decision.title, 72), muted()),
        ]);
    }
    rows.push(TranscriptRow::text(Line::from(header)));

    let last = decision.questions.len().saturating_sub(1);
    for (index, question) in decision.questions.iter().enumerate() {
        let answered = decision_question_answered(decision, question);
        let selected = !decision.answered && index == selected_question;
        let (marker, marker_style) = if selected {
            ("› ", prompt_style())
        } else if answered {
            ("✓ ", success_style())
        } else {
            ("? ", prompt_style())
        };
        rows.push(TranscriptRow::text(Line::from(vec![
            Span::styled(
                if index == last {
                    "  └─ "
                } else {
                    "  ├─ "
                },
                muted(),
            ),
            Span::styled(marker, marker_style),
            Span::styled(
                truncate(&question.prompt, 120),
                if selected {
                    value_style().add_modifier(Modifier::BOLD)
                } else {
                    value_style()
                },
            ),
        ])));
        let continuation = if index == last { "     " } else { "  │  " };
        if question.kind == DecisionQuestionKind::Choice && !decision.answered {
            for (option_index, option) in question.options.iter().take(4).enumerate() {
                let recommended = question.recommended.as_deref() == Some(option.as_str());
                let picked = decision.answers.get(&question.id) == Some(option);
                let mut spans = vec![
                    Span::styled(continuation, muted()),
                    Span::styled(
                        format!("{} {}. ", if picked { "●" } else { "○" }, option_index + 1),
                        if picked { success_style() } else { muted() },
                    ),
                    Span::styled(
                        truncate(option, 100),
                        if picked {
                            success_style()
                        } else {
                            value_style()
                        },
                    ),
                ];
                if recommended {
                    spans.push(Span::styled(" · recommended", muted()));
                }
                rows.push(TranscriptRow::text(Line::from(spans)));
            }
        } else if let Some(answer) = decision.answers.get(&question.id) {
            rows.push(TranscriptRow::text(Line::from(vec![
                Span::styled(continuation, muted()),
                Span::styled(truncate(answer, 120), success_style()),
            ])));
        }
    }
    if let Some(answer) = &decision.answer {
        rows.push(TranscriptRow::text(Line::from(vec![
            Span::styled("  answer ", success_style()),
            Span::styled(truncate(answer, 140), value_style()),
        ])));
    }
}

fn decision_question_answered(decision: &DecisionView, question: &DecisionQuestionView) -> bool {
    decision
        .answers
        .get(&question.id)
        .is_some_and(|answer| !answer.trim().is_empty())
}

fn decision_ready(decision: &DecisionView) -> bool {
    decision
        .questions
        .iter()
        .filter(|question| question.required)
        .all(|question| decision_question_answered(decision, question))
}

fn match_choice_option(options: &[String], value: &str) -> Option<String> {
    let value = value.trim();
    options
        .iter()
        .find(|option| option.eq_ignore_ascii_case(value))
        .cloned()
        .or_else(|| {
            let lower = value.to_ascii_lowercase();
            options
                .iter()
                .find(|option| option.to_ascii_lowercase().starts_with(&lower))
                .cloned()
        })
}

fn decision_answer_text(decision: &DecisionView) -> String {
    let title = if decision.title.trim().is_empty() {
        "planning decision"
    } else {
        decision.title.trim()
    };
    let mut lines = vec![format!("Decision answer: {title}")];
    if !decision.reason.trim().is_empty() {
        lines.push(format!(
            "Reason: {}",
            compact_one_line(&decision.reason, 220)
        ));
    }
    lines.push("Answers:".to_string());
    for question in &decision.questions {
        let answer = decision
            .answers
            .get(&question.id)
            .map(|answer| compact_one_line(answer, 220))
            .unwrap_or_else(|| "(skipped)".to_string());
        lines.push(format!(
            "- {}: {}",
            compact_one_line(&question.id, 48),
            answer
        ));
    }
    if !decision.assumptions.is_empty() {
        lines.push("Assumptions shown:".to_string());
        for assumption in decision.assumptions.iter().take(6) {
            lines.push(format!("- {}", compact_one_line(assumption, 220)));
        }
    }
    lines.join("\n")
}

fn workflow_view_from_plan(
    id: String,
    title: String,
    task: String,
    phases: Vec<WorkflowPhasePlan>,
) -> WorkflowRunView {
    WorkflowRunView {
        id,
        title,
        task,
        status: WorkflowViewState::Running,
        phases: phases
            .into_iter()
            .map(|phase| WorkflowPhaseView {
                name: phase.name,
                objective: phase.objective,
                status: WorkflowViewState::Pending,
                agents: phase
                    .agents
                    .into_iter()
                    .map(|agent| WorkflowAgentView {
                        name: agent.name,
                        role: agent.role,
                        tool_policy: agent.tool_policy,
                        status: WorkflowViewState::Pending,
                        output: String::new(),
                        tool_counts: BTreeMap::new(),
                    })
                    .collect(),
            })
            .collect(),
        summary: String::new(),
        expanded: false,
    }
}

fn workflow_state_from_core(status: WorkflowStatus) -> WorkflowViewState {
    match status {
        WorkflowStatus::Running => WorkflowViewState::Running,
        WorkflowStatus::Succeeded => WorkflowViewState::Succeeded,
        WorkflowStatus::PartiallySucceeded => WorkflowViewState::PartiallySucceeded,
        WorkflowStatus::Failed => WorkflowViewState::Failed,
    }
}

fn append_workflow_rows(
    rows: &mut Vec<TranscriptRow>,
    workflow: &WorkflowRunView,
    context: RenderContext,
) {
    let progress = workflow_progress(workflow);
    let mut header_spans = vec![workflow_state_marker_span(
        workflow.status,
        context.animation_tick,
    )];
    header_spans.push(Span::raw(" "));
    if workflow.status == WorkflowViewState::Running {
        header_spans.extend(light_sweep_spans(
            "workflow",
            context.animation_tick,
            |style| style.add_modifier(Modifier::BOLD),
        ));
    } else {
        header_spans.push(Span::styled("workflow", tool_group_label_style()));
    }
    header_spans.extend([
        Span::styled("  ", muted()),
        Span::styled(truncate(&workflow.title, 56), value_style()),
        Span::styled("  ·  ", muted()),
        Span::styled(workflow_progress_label(progress), muted()),
        Span::styled("  ·  ", muted()),
        Span::styled(
            workflow_state_label(workflow.status),
            workflow_state_style(workflow.status),
        ),
    ]);
    rows.push(TranscriptRow::text(Line::from(header_spans)));

    for phase in workflow.phases.iter().take(5) {
        let phase_progress = workflow_phase_progress(phase);
        rows.push(TranscriptRow::text(Line::from(vec![
            Span::styled("   ", muted()),
            workflow_state_marker_span(phase.status, context.animation_tick),
            Span::raw(" "),
            Span::styled(phase.name.clone(), workflow_state_style(phase.status)),
            Span::styled("  ", tool_group_meta_style()),
            Span::styled(phase_progress, muted()),
            Span::styled("  ·  ", muted()),
            Span::styled(truncate(&phase.objective, 70), muted()),
        ])));

        append_workflow_agent_rows(rows, phase, context);
    }

    if !workflow.summary.trim().is_empty() {
        let preview = workflow
            .summary
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or(workflow.summary.trim());
        rows.push(TranscriptRow::text(Line::from(vec![
            Span::styled("      summary ", tool_group_meta_style()),
            Span::styled(truncate(preview.trim(), 140), muted()),
        ])));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WorkflowProgress {
    phases: usize,
    agents: usize,
    succeeded: usize,
    partial: usize,
    failed: usize,
    running: usize,
    pending: usize,
}

fn workflow_progress(workflow: &WorkflowRunView) -> WorkflowProgress {
    let mut progress = WorkflowProgress {
        phases: workflow.phases.len(),
        agents: 0,
        succeeded: 0,
        partial: 0,
        failed: 0,
        running: 0,
        pending: 0,
    };

    for agent in workflow.phases.iter().flat_map(|phase| &phase.agents) {
        progress.agents += 1;
        match agent.status {
            WorkflowViewState::Pending => progress.pending += 1,
            WorkflowViewState::Running => progress.running += 1,
            WorkflowViewState::Succeeded => progress.succeeded += 1,
            WorkflowViewState::PartiallySucceeded => progress.partial += 1,
            WorkflowViewState::Failed => progress.failed += 1,
        }
    }

    progress
}

fn workflow_phase_progress(phase: &WorkflowPhaseView) -> String {
    let agents = phase.agents.len();
    if agents == 0 {
        return "no agents".to_string();
    }

    let succeeded = phase
        .agents
        .iter()
        .filter(|agent| agent.status == WorkflowViewState::Succeeded)
        .count();
    let partial = phase
        .agents
        .iter()
        .filter(|agent| agent.status == WorkflowViewState::PartiallySucceeded)
        .count();
    let failed = phase
        .agents
        .iter()
        .filter(|agent| agent.status == WorkflowViewState::Failed)
        .count();
    let running = phase
        .agents
        .iter()
        .filter(|agent| agent.status == WorkflowViewState::Running)
        .count();

    if running > 0 {
        format!(
            "{running} running · {}/{} complete",
            succeeded + partial,
            agents
        )
    } else if failed > 0 {
        format!(
            "{failed} failed · {}/{} complete",
            succeeded + partial,
            agents
        )
    } else {
        format!("{}/{} complete", succeeded + partial, agents)
    }
}

fn workflow_progress_label(progress: WorkflowProgress) -> String {
    if progress.agents == 0 {
        return format!("{} phases", progress.phases);
    }

    let completed = progress.succeeded + progress.partial;
    if progress.running > 0 {
        format!(
            "{completed}/{} agents · {} running",
            progress.agents, progress.running
        )
    } else if progress.failed > 0 && completed > 0 {
        format!(
            "{completed}/{} agents · {} failed",
            progress.agents, progress.failed
        )
    } else if progress.failed > 0 {
        format!("{}/{} agents failed", progress.failed, progress.agents)
    } else {
        format!("{completed}/{} agents", progress.agents)
    }
}

fn workflow_latest_activity(workflow: &WorkflowRunView) -> String {
    if let Some(agent) = workflow
        .phases
        .iter()
        .flat_map(|phase| &phase.agents)
        .find(|agent| agent.status == WorkflowViewState::Running)
    {
        return format!(
            "{} running · {}",
            agent.name,
            workflow_agent_tool_summary(agent)
        );
    }

    if let Some(agent) = workflow
        .phases
        .iter()
        .flat_map(|phase| &phase.agents)
        .rev()
        .find(|agent| agent.status == WorkflowViewState::Failed)
    {
        return format!(
            "{} failed · {}",
            agent.name,
            workflow_agent_output_preview(agent).unwrap_or_else(|| agent.role.clone())
        );
    }

    if let Some(preview) = workflow
        .summary
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
    {
        return compact_one_line(preview, 80);
    }

    "waiting for events".to_string()
}

fn workflow_activity_style(workflow: &WorkflowRunView) -> Style {
    if workflow.status == WorkflowViewState::Failed
        || workflow
            .phases
            .iter()
            .flat_map(|phase| &phase.agents)
            .any(|agent| agent.status == WorkflowViewState::Failed)
    {
        error_style()
    } else if workflow.status == WorkflowViewState::Running {
        tool_label_style()
    } else {
        muted()
    }
}

fn append_workflow_agent_rows(
    rows: &mut Vec<TranscriptRow>,
    phase: &WorkflowPhaseView,
    context: RenderContext,
) {
    let salient_agents = workflow_salient_agents(phase);
    let hidden = phase.agents.len().saturating_sub(salient_agents.len());

    for agent in salient_agents {
        rows.push(TranscriptRow::text(workflow_agent_line(agent, context)));
        if agent.status == WorkflowViewState::Failed
            && let Some(preview) = workflow_agent_output_preview(agent)
        {
            rows.push(TranscriptRow::text(Line::from(vec![
                Span::styled("         ", muted()),
                Span::styled(truncate(&preview, 126), error_preview_style()),
            ])));
        }
    }

    if hidden > 0 {
        rows.push(TranscriptRow::text(Line::from(vec![
            Span::styled("      ", muted()),
            Span::styled(format!("+{hidden} more agents"), muted()),
        ])));
    }
}

fn workflow_salient_agents(phase: &WorkflowPhaseView) -> Vec<&WorkflowAgentView> {
    let mut agents = phase
        .agents
        .iter()
        .filter(|agent| {
            matches!(
                agent.status,
                WorkflowViewState::Running | WorkflowViewState::Failed
            )
        })
        .take(3)
        .collect::<Vec<_>>();

    if agents.is_empty()
        && matches!(
            phase.status,
            WorkflowViewState::Succeeded | WorkflowViewState::PartiallySucceeded
        )
    {
        agents.extend(
            phase
                .agents
                .iter()
                .filter(|agent| {
                    matches!(
                        agent.status,
                        WorkflowViewState::Succeeded | WorkflowViewState::PartiallySucceeded
                    )
                })
                .take(1),
        );
    }

    agents
}

fn workflow_agent_line(agent: &WorkflowAgentView, context: RenderContext) -> Line<'static> {
    let state_style = workflow_state_style(agent.status);
    Line::from(vec![
        Span::styled("      |-- ", tool_group_meta_style()),
        workflow_state_marker_span(agent.status, context.animation_tick),
        Span::raw(" "),
        Span::styled(agent.name.clone(), state_style),
        Span::styled(
            format!(" [{}]", subagent_tool_policy_label(agent.tool_policy)),
            muted(),
        ),
        Span::styled("  ", muted()),
        Span::styled(
            workflow_agent_tool_summary(agent),
            message_style(ChatRole::Tool),
        ),
        Span::styled("  ", muted()),
        Span::styled(workflow_state_label(agent.status), state_style),
    ])
}

fn workflow_agent_tool_summary(agent: &WorkflowAgentView) -> String {
    if agent.tool_counts.is_empty() {
        return agent.role.clone();
    }

    agent
        .tool_counts
        .iter()
        .take(4)
        .map(|(name, count)| {
            let name = tool_display_name(name);
            if *count == 1 {
                name.to_string()
            } else {
                format!("{name} x{count}")
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn workflow_agent_output_preview(agent: &WorkflowAgentView) -> Option<String> {
    agent
        .output
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && *line != "done")
        .map(|line| compact_one_line(line, 140))
}

fn workflow_state_marker_span(state: WorkflowViewState, animation_tick: u64) -> Span<'static> {
    match state {
        WorkflowViewState::Pending => Span::styled("·", muted()),
        WorkflowViewState::Running => {
            let frame = animation::ThrobberKind::BrailleOrbit.frame(animation_tick);
            Span::styled(frame.symbol, tool_pulse_style(frame))
        }
        WorkflowViewState::Succeeded => Span::styled("✓", success_style()),
        WorkflowViewState::PartiallySucceeded => Span::styled("◐", prompt_style()),
        WorkflowViewState::Failed => Span::styled("×", error_style()),
    }
}

fn workflow_state_label(state: WorkflowViewState) -> &'static str {
    match state {
        WorkflowViewState::Pending => "queued",
        WorkflowViewState::Running => "running",
        WorkflowViewState::Succeeded => "complete",
        WorkflowViewState::PartiallySucceeded => "partial",
        WorkflowViewState::Failed => "failed",
    }
}

fn subagent_tool_policy_label(policy: SubagentToolPolicy) -> &'static str {
    match policy {
        SubagentToolPolicy::ReadOnly => "read",
        SubagentToolPolicy::ShellRead => "shell-read",
        SubagentToolPolicy::Edit => "edit",
        SubagentToolPolicy::Verify => "verify",
    }
}

fn workflow_state_style(state: WorkflowViewState) -> Style {
    match state {
        WorkflowViewState::Pending => muted(),
        WorkflowViewState::Running => tool_label_style(),
        WorkflowViewState::Succeeded => success_style(),
        WorkflowViewState::PartiallySucceeded => prompt_style(),
        WorkflowViewState::Failed => error_style(),
    }
}

#[cfg(test)]
fn visible_transcript_lines(
    transcript: &[TranscriptItem],
    streaming_message: Option<usize>,
    selected_tool: Option<usize>,
) -> Vec<Line<'static>> {
    transcript_lines_from_rows(&visible_transcript_rows(
        transcript,
        streaming_message,
        selected_tool,
        RenderContext::static_view(),
    ))
}

fn visible_transcript_rows(
    transcript: &[TranscriptItem],
    streaming_message: Option<usize>,
    selected_tool: Option<usize>,
    context: RenderContext,
) -> Vec<TranscriptRow> {
    if transcript.is_empty() {
        return launch_rows();
    }

    let mut rows = Vec::new();
    if should_preserve_launch_rows(transcript, streaming_message) {
        rows.extend(launch_rows());
        rows.push(TranscriptRow::text(Line::from("")));
    }
    let mut index = 0;
    while index < transcript.len() {
        match &transcript[index] {
            TranscriptItem::Message(message) if message.role == ChatRole::Assistant => {
                let assistant = message.clone();
                let assistant_index = index;
                index += 1;

                let activity_start = index;
                while index < transcript.len()
                    && matches!(
                        transcript[index],
                        TranscriptItem::Tool(_) | TranscriptItem::Reasoning(_)
                    )
                {
                    index += 1;
                }

                append_activity_rows(
                    &mut rows,
                    transcript,
                    activity_start,
                    index,
                    selected_tool,
                    context,
                );
                append_chat_message_rows(
                    &mut rows,
                    &assistant,
                    streaming_message == Some(assistant_index),
                );
            }
            TranscriptItem::Message(message) => {
                let is_streaming = streaming_message == Some(index);
                if !rows.is_empty() && message.role == ChatRole::User {
                    rows.push(TranscriptRow::text(Line::from("")));
                }
                append_chat_message_rows(&mut rows, message, is_streaming);
                index += 1;
                if message.role == ChatRole::User {
                    rows.push(TranscriptRow::text(Line::from("")));
                }
            }
            TranscriptItem::Workflow(workflow) => {
                if !rows.is_empty() {
                    rows.push(TranscriptRow::text(Line::from("")));
                }
                append_workflow_rows(&mut rows, workflow, context);
                index += 1;
            }
            // Plans render in the live strip above the composer, not in the
            // transcript; the item stays only as state (persistence + strip).
            TranscriptItem::Plan(_) => {
                index += 1;
            }
            TranscriptItem::Decision(decision) => {
                if !rows.is_empty() {
                    rows.push(TranscriptRow::text(Line::from("")));
                }
                append_decision_rows(&mut rows, decision, context.decision_selection);
                index += 1;
            }
            TranscriptItem::Tool(_) | TranscriptItem::Reasoning(_) => {
                let activity_start = index;
                while index < transcript.len()
                    && matches!(
                        transcript[index],
                        TranscriptItem::Tool(_) | TranscriptItem::Reasoning(_)
                    )
                {
                    index += 1;
                }
                append_activity_rows(
                    &mut rows,
                    transcript,
                    activity_start,
                    index,
                    selected_tool,
                    context,
                );
            }
        }
    }

    append_chat_bottom_padding(&mut rows);
    rows
}

fn append_chat_bottom_padding(rows: &mut Vec<TranscriptRow>) {
    if rows.is_empty() {
        return;
    }

    rows.extend((0..CHAT_BOTTOM_PADDING_ROWS).map(|_| TranscriptRow::text(Line::from(""))));
}

fn should_preserve_launch_rows(
    transcript: &[TranscriptItem],
    streaming_message: Option<usize>,
) -> bool {
    let Some(streaming_index) = streaming_message else {
        return false;
    };
    if transcript.len() > 2 {
        return false;
    }
    let Some(TranscriptItem::Message(first)) = transcript.first() else {
        return false;
    };
    if first.role != ChatRole::User {
        return false;
    }

    matches!(
        transcript.get(streaming_index),
        Some(TranscriptItem::Message(ChatMessage {
            role: ChatRole::Assistant,
            content,
            attachments,
        })) if content.is_empty() && attachments.is_empty()
    )
}

const WORDMARK_WIDE_MIN_COLUMNS: u16 = 64;

fn launch_rows() -> Vec<TranscriptRow> {
    let wide = crossterm::terminal::size()
        .map(|(width, _)| width >= WORDMARK_WIDE_MIN_COLUMNS)
        .unwrap_or(false);

    let mut lines = vec![Line::from("")];
    if wide {
        for art in [
            "  ███╗   ███╗███████╗██████╗ ██╗   ██╗███████╗ █████╗ ",
            "  ████╗ ████║██╔════╝██╔══██╗██║   ██║██╔════╝██╔══██╗",
            "  ██╔████╔██║█████╗  ██║  ██║██║   ██║███████╗███████║",
            "  ██║╚██╔╝██║██╔══╝  ██║  ██║██║   ██║╚════██║██╔══██║",
            "  ██║ ╚═╝ ██║███████╗██████╔╝╚██████╔╝███████║██║  ██║",
        ] {
            lines.push(Line::from(Span::styled(
                art,
                accent().add_modifier(Modifier::BOLD),
            )));
        }
        lines.push(Line::from(vec![
            Span::styled(
                "  ╚═╝     ╚═╝╚══════╝╚═════╝  ╚═════╝ ╚══════╝╚═╝  ╚═╝",
                accent().add_modifier(Modifier::BOLD),
            ),
            Span::styled(concat!("  v", env!("CARGO_PKG_VERSION")), muted()),
        ]));
    } else {
        lines.push(Line::from(Span::styled(
            "  █▀▄▀█ █▀▀ █▀▄ █░█ █▀ ▄▀█",
            accent().add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(vec![
            Span::styled(
                "  █░▀░█ ██▄ █▄▀ █▄█ ▄█ █▀█",
                accent().add_modifier(Modifier::BOLD),
            ),
            Span::styled(concat!("  v", env!("CARGO_PKG_VERSION")), muted()),
        ]));
    }

    lines.extend([
        Line::from(""),
        Line::from(vec![Span::styled(
            "  the coding agent that plans, edits, and verifies",
            value_style(),
        )]),
        Line::from(""),
        Line::from(vec![
            Span::styled("  enter", prompt_style()),
            Span::styled(" send a task", muted()),
            Span::styled("      shift+tab", prompt_style()),
            Span::styled(" plan mode", muted()),
            Span::styled("      ctrl+p", prompt_style()),
            Span::styled(" commands", muted()),
        ]),
        Line::from(vec![
            Span::styled("  ctrl+i", prompt_style()),
            Span::styled(" paste image", muted()),
            Span::styled("    /workflow", prompt_style()),
            Span::styled(" agent fleet", muted()),
            Span::styled("     esc esc", prompt_style()),
            Span::styled(" quit", muted()),
        ]),
        Line::from(""),
    ]);

    lines.into_iter().map(TranscriptRow::text).collect()
}

fn transcript_lines_from_rows(rows: &[TranscriptRow]) -> Vec<Line<'static>> {
    rows.iter().map(|row| row.line.clone()).collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TranscriptImagePlacement {
    attachment: ImageAttachment,
    width: u16,
    height: u16,
    x_offset: u16,
    y_offset: i16,
}

fn transcript_image_placements(
    rows: &[TranscriptRow],
    area: Rect,
    top_offset: usize,
) -> Vec<TranscriptImagePlacement> {
    if area.width < 8 || area.height == 0 {
        return Vec::new();
    }

    let x_offset = 2;
    let image_width = CHAT_IMAGE_PREVIEW_WIDTH.min(area.width.saturating_sub(x_offset));
    if image_width == 0 {
        return Vec::new();
    }

    let viewport_bottom = top_offset.saturating_add(area.height as usize);
    let mut placements = Vec::new();
    let mut visual_start = 0usize;

    for row in rows {
        if let Some(attachment) = &row.image {
            let image_height = CHAT_IMAGE_PREVIEW_HEIGHT;
            let image_bottom = visual_start.saturating_add(image_height as usize);
            if image_bottom > top_offset && visual_start < viewport_bottom {
                placements.push(TranscriptImagePlacement {
                    attachment: attachment.clone(),
                    width: image_width,
                    height: image_height,
                    x_offset,
                    y_offset: signed_visual_offset(visual_start, top_offset),
                });
            }
        }

        visual_start = visual_start.saturating_add(row_visual_height(row, area.width));
        if visual_start >= viewport_bottom {
            break;
        }
    }

    placements
}

fn signed_visual_offset(visual_start: usize, top_offset: usize) -> i16 {
    if visual_start >= top_offset {
        visual_start
            .saturating_sub(top_offset)
            .min(i16::MAX as usize) as i16
    } else {
        -(top_offset
            .saturating_sub(visual_start)
            .min(i16::MAX as usize) as i16)
    }
}

#[derive(Debug, Clone)]
struct TranscriptViewportWindow {
    rows: Vec<TranscriptRow>,
    scroll_offset: usize,
}

fn transcript_viewport_window(
    rows: &[TranscriptRow],
    width: u16,
    top_offset: usize,
    viewport_height: usize,
) -> TranscriptViewportWindow {
    if rows.is_empty() || viewport_height == 0 {
        return TranscriptViewportWindow {
            rows: Vec::new(),
            scroll_offset: 0,
        };
    }

    let mut skipped_visual_rows = 0usize;
    let mut visible_visual_rows = 0usize;
    let mut scroll_offset = 0usize;
    let mut visible_rows = Vec::new();
    let mut taking = false;

    for row in rows {
        let visual_rows = row_visual_height(row, width);
        if !taking {
            if skipped_visual_rows.saturating_add(visual_rows) <= top_offset {
                skipped_visual_rows = skipped_visual_rows.saturating_add(visual_rows);
                continue;
            }
            scroll_offset = top_offset.saturating_sub(skipped_visual_rows);
            taking = true;
        }

        visible_rows.push(row.clone());
        visible_visual_rows = visible_visual_rows.saturating_add(visual_rows);
        if visible_visual_rows.saturating_sub(scroll_offset) >= viewport_height {
            break;
        }
    }

    TranscriptViewportWindow {
        rows: visible_rows,
        scroll_offset,
    }
}

fn chat_viewport_metrics(
    rows: &[TranscriptRow],
    area: Rect,
    requested_scroll: usize,
) -> ChatViewportMetrics {
    let text_area = area;
    let total_visual_lines = wrapped_row_count(rows, text_area.width);
    let has_scrollbar = area.width > 4 && total_visual_lines > area.height as usize;
    let max_scroll = total_visual_lines.saturating_sub(area.height as usize);
    let scroll = requested_scroll.min(max_scroll);
    let top_offset = max_scroll.saturating_sub(scroll);

    ChatViewportMetrics {
        text_area,
        has_scrollbar,
        total_visual_lines,
        max_scroll,
        scroll,
        top_offset,
    }
}

fn scroll_progress_percent(metrics: &ChatViewportMetrics) -> usize {
    if metrics.max_scroll == 0 {
        return 100;
    }
    metrics.top_offset.saturating_mul(100) / metrics.max_scroll
}

fn paragraph_scroll_offset(top_offset: usize) -> u16 {
    top_offset.min(u16::MAX as usize) as u16
}

fn wrapped_row_count(rows: &[TranscriptRow], width: u16) -> usize {
    rows.iter().map(|row| row_visual_height(row, width)).sum()
}

fn row_visual_height(row: &TranscriptRow, width: u16) -> usize {
    let width = width.max(1) as usize;
    if row.image.is_some() {
        1
    } else {
        line_width(&row.line).max(1).div_ceil(width)
    }
}

#[cfg(test)]
fn trim_wrapped_lines_for_viewport(
    rows: &[TranscriptRow],
    width: u16,
    skip_rows: usize,
    viewport_height: usize,
) -> Vec<TranscriptRow> {
    transcript_viewport_window(rows, width, skip_rows, viewport_height).rows
}

fn line_width(line: &Line<'_>) -> usize {
    line.spans
        .iter()
        .map(|span| span.content.chars().map(char_display_width).sum::<usize>())
        .sum()
}

fn char_display_width(ch: char) -> usize {
    if ch == '\n' || ch == '\r' || ch == '\t' {
        1
    } else if ch.is_control() {
        0
    } else {
        1
    }
}

fn input_display_lines(input: &str, cursor: usize, max_lines: usize) -> Vec<Line<'static>> {
    if input.is_empty() {
        return vec![Line::from(vec![
            Span::styled("█", cursor_style()),
            Span::styled(" Type a task or ask a question…", placeholder_style()),
        ])];
    }

    let max_lines = max_lines.max(1);
    let visible_start_line = cursor_line(input, cursor)
        .saturating_add(1)
        .saturating_sub(max_lines);

    let mut lines = Vec::new();
    let mut current = Vec::new();
    let mut current_line = 0usize;

    for (index, ch) in input.chars().enumerate() {
        if current_line >= visible_start_line && index == cursor {
            current.push(Span::styled("█", cursor_style()));
        }

        if ch == '\n' {
            if current_line >= visible_start_line {
                lines.push(Line::from(current));
                current = Vec::new();
                if lines.len() >= max_lines {
                    return lines;
                }
            }
            current_line += 1;
        } else {
            if current_line >= visible_start_line {
                current.push(Span::styled(ch.to_string(), value_style()));
            }
        }
    }

    if current_line >= visible_start_line && cursor == input.chars().count() {
        current.push(Span::styled("█", cursor_style()));
    }

    if current_line >= visible_start_line && lines.len() < max_lines {
        lines.push(Line::from(current));
    }
    lines
}

fn cursor_line(input: &str, cursor: usize) -> usize {
    input.chars().take(cursor).filter(|ch| *ch == '\n').count()
}

fn vertically_center_input_lines(
    mut lines: Vec<Line<'static>>,
    available_content_height: u16,
) -> Vec<Line<'static>> {
    let available = available_content_height as usize;
    if available <= lines.len() {
        return lines;
    }

    let top_padding = (available - lines.len()).div_ceil(2);
    if top_padding == 0 {
        return lines;
    }

    let mut centered = vec![Line::from(""); top_padding];
    centered.append(&mut lines);
    centered
}

fn attachment_strip_line(attachments: &[ImageAttachment]) -> Line<'static> {
    let mut spans = vec![Span::styled("  ", muted())];
    for (index, attachment) in attachments.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw(" "));
        }
        spans.push(Span::styled(
            format!("󰋩 {}", attachment_label(attachment)),
            Style::default()
                .fg(palette().success)
                .bg(palette().inline_code_bg)
                .add_modifier(Modifier::BOLD),
        ));
    }
    spans.push(Span::styled("  ctrl+d detach latest", muted()));
    Line::from(spans)
}

fn composer_attachment_preview_lines(
    attachments: &[ImageAttachment],
    previews: &HashMap<String, Vec<Line<'static>>>,
    available_width: u16,
) -> Vec<Line<'static>> {
    if attachments.is_empty() || available_width == 0 {
        return Vec::new();
    }

    let cell_width = COMPOSER_IMAGE_PREVIEW_WIDTH as usize;
    let gap_width = 1usize;
    let usable_width = available_width.saturating_sub(4) as usize;
    let max_cells = (usable_width + gap_width)
        .checked_div(cell_width + gap_width)
        .unwrap_or(1)
        .max(1);
    let visible_count = attachments.len().min(max_cells);
    let hidden_count = attachments.len().saturating_sub(visible_count);
    let mut rows = vec![Vec::new(); COMPOSER_IMAGE_PREVIEW_HEIGHT as usize];

    for attachment in attachments.iter().take(visible_count) {
        let fallback;
        let preview = if let Some(preview) = previews.get(&attachment.id) {
            preview
        } else {
            fallback = image_preview_lines(attachment, COMPOSER_IMAGE_PREVIEW_WIDTH);
            &fallback
        };

        for (row_index, row) in rows.iter_mut().enumerate() {
            if !row.is_empty() {
                row.push(Span::raw(" "));
            }
            if let Some(line) = preview.get(row_index) {
                row.extend(line.spans.clone());
            } else {
                row.push(Span::raw(" ".repeat(cell_width)));
            }
        }
    }

    if hidden_count > 0
        && let Some(first_row) = rows.first_mut()
    {
        first_row.push(Span::raw(" "));
        first_row.push(Span::styled(
            format!("+{hidden_count}"),
            attachment_preview_meta_style(),
        ));
    }

    rows.into_iter()
        .map(|spans| {
            let mut prefixed = vec![Span::styled("  ", muted())];
            prefixed.extend(spans);
            Line::from(prefixed)
        })
        .collect()
}

fn image_preview_lines(attachment: &ImageAttachment, width: u16) -> Vec<Line<'static>> {
    image_placeholder_lines(attachment, width, COMPOSER_IMAGE_PREVIEW_HEIGHT)
}

fn image_input_warning(provider: &str) -> Option<&'static str> {
    if provider == "codex" {
        None
    } else {
        Some("current backend sends a placeholder instead of image pixels")
    }
}

fn preview_image_dimensions(attachment: &ImageAttachment, area: Rect, zoom: u16) -> (u16, u16) {
    if area.width == 0 || area.height == 0 {
        return (0, 0);
    }

    let image_width = attachment.width.max(1) as f64;
    let image_height = attachment.height.max(1) as f64;
    let fit_height_from_width = ((area.width as f64 * image_height / image_width) * 0.5)
        .ceil()
        .max(1.0) as u16;

    let (fit_width, fit_height) = if fit_height_from_width <= area.height {
        (area.width, fit_height_from_width)
    } else {
        let width = ((area.height as f64 * image_width / image_height) * 2.0)
            .ceil()
            .max(1.0) as u16;
        (width.min(area.width), area.height)
    };

    let zoom = zoom.clamp(IMAGE_PREVIEW_MIN_ZOOM, IMAGE_PREVIEW_MAX_ZOOM) as u32;
    let width = ((fit_width as u32).saturating_mul(zoom) / 100).clamp(1, u16::MAX as u32) as u16;
    let height = ((fit_height as u32).saturating_mul(zoom) / 100).clamp(1, u16::MAX as u32) as u16;

    (width, height)
}

fn image_placeholder_lines(
    attachment: &ImageAttachment,
    width: u16,
    height: u16,
) -> Vec<Line<'static>> {
    let width = width.max(10) as usize;
    let height = height.max(3) as usize;
    let inner_width = width.saturating_sub(2);
    let border = "─".repeat(inner_width);
    let dimensions = format!("{}×{}", attachment.width, attachment.height);
    let size = human_bytes(attachment.size_bytes);
    let name = truncate(&attachment.name, inner_width);
    let mut lines = vec![
        Line::from(Span::styled(
            format!("╭{border}╮"),
            attachment_preview_border_style(),
        )),
        attachment_preview_body_line("image", width, attachment_preview_title_style()),
        attachment_preview_body_line(&dimensions, width, attachment_preview_meta_style()),
    ];

    while lines.len() + 2 < height {
        lines.push(attachment_preview_body_line("", width, muted()));
    }

    lines.push(attachment_preview_body_line(
        &format!("{name} {size}"),
        width,
        muted(),
    ));
    lines.push(Line::from(Span::styled(
        format!("╰{border}╯"),
        attachment_preview_border_style(),
    )));
    lines
}

fn attachment_preview_body_line(text: &str, width: usize, style: Style) -> Line<'static> {
    let inner_width = width.saturating_sub(2);
    let fitted = truncate(text, inner_width);
    let padding = inner_width.saturating_sub(fitted.chars().count());
    Line::from(vec![
        Span::styled("│", attachment_preview_border_style()),
        Span::styled(fitted, style),
        Span::raw(" ".repeat(padding)),
        Span::styled("│", attachment_preview_border_style()),
    ])
}

fn append_chat_message_rows(
    rows: &mut Vec<TranscriptRow>,
    message: &ChatMessage,
    is_streaming: bool,
) {
    if message.role == ChatRole::User {
        append_user_message_rows(rows, &message.content, &message.attachments);
        return;
    }

    let mut lines = Vec::new();
    append_chat_message_lines(&mut lines, message, is_streaming);
    rows.extend(lines.into_iter().map(TranscriptRow::text));
}

fn append_chat_message_lines(
    lines: &mut Vec<Line<'static>>,
    message: &ChatMessage,
    is_streaming: bool,
) {
    if message.role == ChatRole::User {
        append_user_message_lines(lines, &message.content, &message.attachments);
        return;
    }

    if message.role == ChatRole::System {
        for line in message
            .content
            .lines()
            .filter(|line| !line.trim().is_empty())
        {
            lines.push(Line::from(vec![
                Span::styled("  · ", muted()),
                Span::styled(line.trim().to_string(), muted()),
            ]));
        }
        return;
    }

    if message.content.is_empty() && is_streaming {
        return;
    }

    let mut rendered = markdown_content_lines(&message.content, message.role);
    if is_streaming {
        if let Some(last) = rendered.last_mut() {
            last.spans.push(Span::styled("█", cursor_style()));
        } else {
            rendered.push(Line::from(Span::styled("  █", cursor_style())));
        }
    }

    lines.extend(rendered);
}

fn append_user_message_rows(
    rows: &mut Vec<TranscriptRow>,
    content: &str,
    attachments: &[ImageAttachment],
) {
    let mut lines = Vec::new();
    append_user_message_lines(&mut lines, content, attachments);
    rows.extend(lines.into_iter().map(TranscriptRow::text));

    for attachment in attachments {
        let preview = image_placeholder_lines(
            attachment,
            CHAT_IMAGE_PREVIEW_WIDTH,
            CHAT_IMAGE_PREVIEW_HEIGHT,
        );
        for (index, line) in preview.into_iter().enumerate() {
            if index == 0 {
                rows.push(TranscriptRow::image(line, attachment.clone()));
            } else {
                rows.push(TranscriptRow::text(line));
            }
        }
    }
}

fn append_user_message_lines(
    lines: &mut Vec<Line<'static>>,
    content: &str,
    attachments: &[ImageAttachment],
) {
    let base_style = user_message_style();
    let prompt_style = user_message_prompt_style();

    if content.trim().is_empty() {
        lines.push(Line::from(vec![
            Span::styled(" › ", prompt_style),
            Span::styled(" ", base_style),
        ]));
        return;
    }

    for (index, raw_line) in content.lines().enumerate() {
        let marker = if index == 0 { " › " } else { "   " };
        let mut spans = vec![Span::styled(marker, prompt_style)];
        spans.extend(inline_markdown_spans(raw_line.trim_end(), base_style));
        spans.push(Span::styled(" ", base_style));
        lines.push(Line::from(spans));
    }

    if !attachments.is_empty() {
        lines.push(attachment_strip_line(attachments).style(user_message_background_style()));
    }
}

fn code_syntaxes() -> &'static syntect::parsing::SyntaxSet {
    static SYNTAXES: OnceLock<syntect::parsing::SyntaxSet> = OnceLock::new();
    SYNTAXES.get_or_init(syntect::parsing::SyntaxSet::load_defaults_newlines)
}

fn code_theme() -> &'static syntect::highlighting::Theme {
    static THEME: OnceLock<syntect::highlighting::Theme> = OnceLock::new();
    THEME.get_or_init(|| {
        // Muted base16 palette that sits well on all of Medusa's dark themes.
        // Only foreground colors are used; the terminal background shows through.
        syntect::highlighting::ThemeSet::load_defaults()
            .themes
            .remove("base16-eighties.dark")
            .expect("syntect default themes include base16-eighties.dark")
    })
}

struct CodeHighlighter {
    inner: syntect::easy::HighlightLines<'static>,
}

impl CodeHighlighter {
    /// None when the fence has no language tag or we don't know the syntax —
    /// the block then renders in the plain code style.
    fn for_language(token: &str) -> Option<Self> {
        let token = token.trim();
        if token.is_empty() {
            return None;
        }
        let syntaxes = code_syntaxes();
        let syntax = syntaxes
            .find_syntax_by_token(token)
            .or_else(|| syntaxes.find_syntax_by_extension(token))?;
        Some(Self {
            inner: syntect::easy::HighlightLines::new(syntax, code_theme()),
        })
    }

    fn spans(&mut self, line: &str) -> Vec<Span<'static>> {
        let with_newline = format!("{line}\n");
        let Ok(regions) = self.inner.highlight_line(&with_newline, code_syntaxes()) else {
            return vec![Span::styled(line.to_string(), code_block_style())];
        };
        regions
            .into_iter()
            .map(|(style, text)| {
                let fg = style.foreground;
                Span::styled(
                    text.trim_end_matches('\n').to_string(),
                    Style::default().fg(Color::Rgb(fg.r, fg.g, fg.b)),
                )
            })
            .filter(|span| !span.content.is_empty())
            .collect()
    }
}

/// Memoized front for [`markdown_content_lines_uncached`]. During streaming
/// every delta invalidates the whole-transcript row cache, which would
/// re-render (and re-highlight) every historical message per frame; this keeps
/// that cost to the one message actually changing.
fn markdown_content_lines(content: &str, role: ChatRole) -> Vec<Line<'static>> {
    use std::hash::{Hash, Hasher};

    const MARKDOWN_CACHE_CAP: usize = 512;
    static CACHE: OnceLock<std::sync::Mutex<HashMap<u64, Vec<Line<'static>>>>> = OnceLock::new();

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    content.hash(&mut hasher);
    (role as u8).hash(&mut hasher);
    ACTIVE_THEME.load(Ordering::Relaxed).hash(&mut hasher);
    let key = hasher.finish();

    let cache = CACHE.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    if let Ok(cache) = cache.lock()
        && let Some(lines) = cache.get(&key)
    {
        return lines.clone();
    }

    let lines = markdown_content_lines_uncached(content, role);
    if let Ok(mut cache) = cache.lock() {
        // Streaming generates a new key per delta; a full reset at the cap is
        // fine because live entries repopulate on the next frame.
        if cache.len() >= MARKDOWN_CACHE_CAP {
            cache.clear();
        }
        cache.insert(key, lines.clone());
    }
    lines
}

fn markdown_content_lines_uncached(content: &str, role: ChatRole) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut in_code_block = false;
    let mut highlighter: Option<CodeHighlighter> = None;

    for raw_line in content.lines() {
        let line = raw_line.trim_end();
        let trimmed = line.trim_start();

        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_code_block = !in_code_block;
            highlighter = if in_code_block {
                CodeHighlighter::for_language(&trimmed[3..])
            } else {
                None
            };
            continue;
        }

        if in_code_block {
            let mut spans = vec![Span::styled("  │ ", code_border_style())];
            match highlighter.as_mut() {
                Some(highlighter) => spans.extend(highlighter.spans(line)),
                None => spans.push(Span::styled(line.to_string(), code_block_style())),
            }
            lines.push(Line::from(spans));
            continue;
        }

        if trimmed.is_empty() {
            lines.push(Line::from(""));
            continue;
        }

        if is_horizontal_rule(trimmed) {
            lines.push(Line::from(vec![
                Span::styled("  ", muted()),
                Span::styled("─".repeat(48), separator_style()),
            ]));
            continue;
        }

        if let Some((level, heading)) = parse_heading(trimmed) {
            lines.push(Line::from(vec![
                Span::styled("  ", muted()),
                Span::styled(heading.to_string(), heading_style(level)),
            ]));
            continue;
        }

        if let Some(quote) = trimmed.strip_prefix("> ") {
            let mut spans = vec![
                Span::styled("  ┃ ", quote_border_style()),
                Span::styled("", quote_style()),
            ];
            spans.extend(inline_markdown_spans(quote, quote_style()));
            lines.push(Line::from(spans));
            continue;
        }

        if let Some((indent, marker, body)) = parse_list_item(line) {
            let mut spans = vec![
                Span::styled("  ", muted()),
                Span::raw("  ".repeat(indent)),
                Span::styled(marker, list_marker_style()),
                Span::raw(" "),
            ];
            spans.extend(inline_markdown_spans(body, message_style(role)));
            lines.push(Line::from(spans));
            continue;
        }

        let mut spans = vec![Span::styled("  ", muted())];
        spans.extend(inline_markdown_spans(trimmed, message_style(role)));
        lines.push(Line::from(spans));
    }

    lines
}

fn parse_heading(line: &str) -> Option<(usize, &str)> {
    let level = line.chars().take_while(|ch| *ch == '#').count();
    if !(1..=6).contains(&level) {
        return None;
    }

    let rest = line.get(level..)?;
    if !rest.starts_with(' ') {
        return None;
    }

    Some((level, rest.trim()))
}

fn parse_list_item(line: &str) -> Option<(usize, String, &str)> {
    let leading_spaces = line.chars().take_while(|ch| *ch == ' ').count();
    let indent = leading_spaces / 2;
    let trimmed = line.trim_start();

    for marker in ["- ", "* ", "+ "] {
        if let Some(body) = trimmed.strip_prefix(marker) {
            return Some((indent, "•".to_string(), body.trim()));
        }
    }

    let dot = trimmed.find(". ")?;
    let number = &trimmed[..dot];
    if !number.chars().all(|ch| ch.is_ascii_digit()) {
        return None;
    }

    Some((indent, format!("{number}."), trimmed[dot + 2..].trim()))
}

fn is_horizontal_rule(line: &str) -> bool {
    let chars = line.chars().collect::<Vec<_>>();
    chars.len() >= 3 && chars.iter().all(|ch| matches!(ch, '-' | '*' | '_'))
}

fn inline_markdown_spans(text: &str, base_style: Style) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut rest = text;

    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix('`')
            && let Some(end) = after.find('`')
        {
            spans.push(Span::styled(after[..end].to_string(), inline_code_style()));
            rest = &after[end + 1..];
            continue;
        }

        if let Some(after) = rest.strip_prefix("**")
            && let Some(end) = after.find("**")
        {
            spans.push(Span::styled(
                after[..end].to_string(),
                base_style.add_modifier(Modifier::BOLD),
            ));
            rest = &after[end + 2..];
            continue;
        }

        if let Some(after) = rest.strip_prefix("__")
            && let Some(end) = after.find("__")
        {
            spans.push(Span::styled(
                after[..end].to_string(),
                base_style.add_modifier(Modifier::BOLD),
            ));
            rest = &after[end + 2..];
            continue;
        }

        if let Some(after) = rest.strip_prefix('*')
            && let Some(end) = after.find('*')
        {
            spans.push(Span::styled(
                after[..end].to_string(),
                base_style.add_modifier(Modifier::ITALIC),
            ));
            rest = &after[end + 1..];
            continue;
        }

        if let Some(after) = rest.strip_prefix('_')
            && let Some(end) = after.find('_')
        {
            spans.push(Span::styled(
                after[..end].to_string(),
                base_style.add_modifier(Modifier::ITALIC),
            ));
            rest = &after[end + 1..];
            continue;
        }

        if let Some(after_open) = rest.strip_prefix('[')
            && let Some(close) = after_open.find("](")
        {
            let label = &after_open[..close];
            let after_label = &after_open[close + 2..];
            if let Some(end_url) = after_label.find(')') {
                spans.push(Span::styled(label.to_string(), link_style()));
                rest = &after_label[end_url + 1..];
                continue;
            }
        }

        let next = next_inline_marker(rest).unwrap_or(rest.len()).max(1);
        spans.push(Span::styled(rest[..next].to_string(), base_style));
        rest = &rest[next..];
    }

    spans
}

fn next_inline_marker(text: &str) -> Option<usize> {
    ["`", "**", "__", "*", "_", "["]
        .iter()
        .filter_map(|marker| text.find(marker))
        .filter(|index| *index > 0)
        .min()
}

fn append_activity_rows(
    rows: &mut Vec<TranscriptRow>,
    transcript: &[TranscriptItem],
    start: usize,
    end: usize,
    selected_tool: Option<usize>,
    context: RenderContext,
) {
    let mut lines = Vec::new();
    append_activity_lines(&mut lines, transcript, start, end, selected_tool, context);
    rows.extend(lines.into_iter().map(TranscriptRow::text));
}

fn append_activity_lines(
    lines: &mut Vec<Line<'static>>,
    transcript: &[TranscriptItem],
    start: usize,
    end: usize,
    selected_tool: Option<usize>,
    context: RenderContext,
) {
    let has_tools = transcript[start..end]
        .iter()
        .any(|item| matches!(item, TranscriptItem::Tool(_)));

    if !has_tools {
        return;
    }

    append_tool_group_lines(lines, transcript, start, end, selected_tool, context);
}

fn tool_group_is_open(transcript: &[TranscriptItem], start: usize, end: usize) -> bool {
    transcript[start..end]
        .iter()
        .find_map(|item| match item {
            TranscriptItem::Tool(run) => Some(run.group_expanded),
            _ => None,
        })
        .unwrap_or(false)
}

/// The tool verb without the redundant leading name, e.g. "read src/main.rs" -> "src/main.rs".
fn tool_summary_rest(run: &ToolRun) -> String {
    let name = tool_display_name(&run.name);
    let summary = tool_summary(&run.summary);
    summary
        .strip_prefix(name)
        .map(str::trim_start)
        .unwrap_or(summary.as_str())
        .to_string()
}

/// Edits and patches carry diffs the user should see per-call; never merge them.
fn tool_name_coalescible(name: &str) -> bool {
    !matches!(name, "file.edit" | "file.patch")
}

/// True when the group contains at least one coalescible run — consecutive
/// succeeded calls to the same tool (reasoning items in between don't break a run).
fn tool_group_has_coalesced_runs(transcript: &[TranscriptItem], start: usize, end: usize) -> bool {
    let mut previous: Option<&str> = None;
    for item in &transcript[start..end] {
        match item {
            TranscriptItem::Tool(run)
                if run.state == ToolRunState::Succeeded && tool_name_coalescible(&run.name) =>
            {
                let name = tool_display_name(&run.name);
                if previous == Some(name) {
                    return true;
                }
                previous = Some(name);
            }
            TranscriptItem::Tool(_) => previous = None,
            _ => {}
        }
    }
    false
}

const TOOL_COALESCE_SHOWN_TARGETS: usize = 3;

fn append_coalesced_tool_lines(
    lines: &mut Vec<Line<'static>>,
    runs: &[&ToolRun],
    selected: bool,
    context: RenderContext,
) {
    let selection = if selected {
        activity_selected_style()
    } else {
        Style::default()
    };
    let sel = |style: Style| style.patch(selection);

    let name = tool_display_name(&runs[0].name);
    // A running call can only ever be the tail of a coalesced run.
    let active = runs
        .last()
        .filter(|run| run.state == ToolRunState::Running)
        .copied();
    let targets: Vec<String> = runs
        .iter()
        .filter(|run| run.state == ToolRunState::Succeeded)
        .map(|run| tool_summary_rest(run))
        .filter(|target| !target.is_empty())
        .collect();
    let extra = targets.len().saturating_sub(TOOL_COALESCE_SHOWN_TARGETS);
    let mut label = targets
        .iter()
        .take(TOOL_COALESCE_SHOWN_TARGETS)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if extra > 0 {
        label.push_str(&format!(" +{extra} more"));
    }
    if let Some(active) = active {
        let target = tool_summary_rest(active);
        if !target.is_empty() {
            if !label.is_empty() {
                label.push_str(", ");
            }
            label.push_str(&target);
        }
    }
    if label.is_empty() {
        label = format!("×{}", runs.len());
    }

    let marker = if active.is_some() {
        tool_running_marker_span(ToolRunState::Running, context.animation_tick)
    } else {
        Span::styled(TOOL_MARKER, sel(tool_marker_style()))
    };
    lines.push(Line::from(vec![
        marker,
        Span::raw(" "),
        Span::styled(
            name.to_string(),
            sel(tool_label_style().add_modifier(Modifier::BOLD)),
        ),
        Span::styled(
            format!(" {}", truncate(&label, 140)),
            sel(message_style(ChatRole::Tool)),
        ),
    ]));

    let mut result = vec![
        Span::raw("  "),
        Span::styled("⎿ ", separator_style()),
        Span::styled(format!("{} calls", runs.len()), muted()),
    ];
    if active.is_some() {
        result.push(Span::styled(" · running…", muted()));
    }
    if selected {
        result.push(Span::styled(" · enter to expand", muted()));
    }
    lines.push(Line::from(result));
}

fn append_tool_group_lines(
    lines: &mut Vec<Line<'static>>,
    transcript: &[TranscriptItem],
    start: usize,
    end: usize,
    selected_tool: Option<usize>,
    context: RenderContext,
) {
    let coalesce = !tool_group_is_open(transcript, start, end);
    let mut first = true;
    let mut index = start;
    while index < end {
        let TranscriptItem::Tool(run) = &transcript[index] else {
            index += 1;
            continue;
        };

        // Collect the consecutive run of succeeded calls to the same tool,
        // skipping reasoning items in between.
        let mut matched: Vec<&ToolRun> = vec![run];
        let mut cursor = index + 1;
        if coalesce
            && run.state == ToolRunState::Succeeded
            && !run.expanded
            && tool_name_coalescible(&run.name)
        {
            loop {
                let mut probe = cursor;
                while probe < end && matches!(transcript[probe], TranscriptItem::Reasoning(_)) {
                    probe += 1;
                }
                match transcript.get(probe) {
                    Some(TranscriptItem::Tool(next))
                        if probe < end
                            && next.state == ToolRunState::Succeeded
                            && !next.expanded
                            && tool_display_name(&next.name) == tool_display_name(&run.name) =>
                    {
                        matched.push(next);
                        cursor = probe + 1;
                    }
                    // A running call of the same tool joins as the live tail, so
                    // it doesn't render below only to jump into the run on success.
                    Some(TranscriptItem::Tool(next))
                        if probe < end
                            && next.state == ToolRunState::Running
                            && tool_display_name(&next.name) == tool_display_name(&run.name) =>
                    {
                        matched.push(next);
                        cursor = probe + 1;
                        break;
                    }
                    _ => break,
                }
            }
        }

        if !first {
            lines.push(Line::from(""));
        }
        first = false;

        if matched.len() > 1 {
            let selected = matches!(selected_tool, Some(sel) if sel >= index && sel < cursor);
            append_coalesced_tool_lines(lines, &matched, selected, context);
            index = cursor;
        } else {
            append_tool_call_lines(lines, run, selected_tool == Some(index), context);
            index += 1;
        }
    }
}

const TOOL_DETAIL_COLLAPSED_LINES: usize = 1;
const TOOL_DETAIL_FAILED_LINES: usize = 4;
const TOOL_DETAIL_EXPANDED_LINES: usize = 24;
/// Diffs are the payoff of an edit — show a real chunk of them by default.
const TOOL_DETAIL_DIFF_COLLAPSED_LINES: usize = 12;
const TOOL_DETAIL_DIFF_EXPANDED_LINES: usize = 64;

fn append_tool_call_lines(
    lines: &mut Vec<Line<'static>>,
    run: &ToolRun,
    selected: bool,
    context: RenderContext,
) {
    let selection = if selected {
        activity_selected_style()
    } else {
        Style::default()
    };
    let sel = |style: Style| style.patch(selection);

    let marker = match run.state {
        ToolRunState::Running => tool_running_marker_span(run.state, context.animation_tick),
        ToolRunState::Succeeded => Span::styled(TOOL_MARKER, sel(tool_marker_style())),
        ToolRunState::Failed => Span::styled(TOOL_MARKER, sel(error_style())),
    };

    let mut row = vec![marker, Span::raw(" ")];
    let name = tool_display_name(&run.name);
    // Summaries like "read AGENTS.md" already start with the tool verb, so
    // drop the redundant name to avoid "read read AGENTS.md".
    let summary = tool_summary(&run.summary);
    let summary_rest = summary
        .strip_prefix(name)
        .map(str::trim_start)
        .unwrap_or(summary.as_str());
    row.push(Span::styled(
        name.to_string(),
        sel(tool_label_style().add_modifier(Modifier::BOLD)),
    ));
    if !summary_rest.is_empty() {
        row.push(Span::styled(
            format!(" {}", truncate(summary_rest, 140)),
            sel(message_style(ChatRole::Tool)),
        ));
    }
    lines.push(Line::from(row));

    if run.state == ToolRunState::Running {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled("⎿ ", separator_style()),
            Span::styled("running…", muted()),
        ]));
        return;
    }

    let detail_lines = meaningful_tool_output_lines(run);
    if detail_lines.is_empty() {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled("⎿ ", separator_style()),
            match run.state {
                ToolRunState::Failed => Span::styled("failed", error_style()),
                _ => Span::styled("done", muted()),
            },
        ]));
        return;
    }

    let has_diff = tool_run_has_diff(run);
    let visible = if run.expanded {
        if has_diff {
            TOOL_DETAIL_DIFF_EXPANDED_LINES
        } else {
            TOOL_DETAIL_EXPANDED_LINES
        }
    } else if run.state == ToolRunState::Failed {
        TOOL_DETAIL_FAILED_LINES
    } else if has_diff {
        TOOL_DETAIL_DIFF_COLLAPSED_LINES
    } else {
        TOOL_DETAIL_COLLAPSED_LINES
    };
    let body_style = match run.state {
        ToolRunState::Failed => error_preview_style(),
        _ => muted(),
    };

    for (index, line) in detail_lines.iter().take(visible).enumerate() {
        let prefix = if index == 0 { "⎿ " } else { "  " };
        let mut row = vec![Span::raw("  "), Span::styled(prefix, separator_style())];
        if line.contains('\u{1b}') {
            row.extend(ansi_detail_spans(line, body_style));
        } else {
            let line_style = if has_diff && run.state != ToolRunState::Failed {
                diff_line_style(line).unwrap_or(body_style)
            } else {
                body_style
            };
            row.push(Span::styled(truncate(line, 170), line_style));
        }
        lines.push(Line::from(row));
    }

    let hidden = detail_lines.len().saturating_sub(visible);
    if hidden > 0 {
        let hint = if run.expanded {
            format!(
                "… +{hidden} more line{}",
                if hidden == 1 { "" } else { "s" }
            )
        } else {
            format!(
                "… +{hidden} line{} (enter to expand)",
                if hidden == 1 { "" } else { "s" }
            )
        };
        lines.push(Line::from(vec![
            Span::raw("    "),
            Span::styled(hint, muted()),
        ]));
    }
}

fn tool_running_marker_span(state: ToolRunState, animation_tick: u64) -> Span<'static> {
    match state {
        ToolRunState::Running => {
            let frame = animation::ThrobberKind::ToolPulse.frame(animation_tick);
            Span::styled(frame.symbol, tool_pulse_style(frame))
        }
        ToolRunState::Succeeded => Span::styled("✓", success_style()),
        ToolRunState::Failed => Span::styled("×", error_style()),
    }
}

fn tool_pulse_style(frame: animation::ThrobberFrame) -> Style {
    match frame.energy {
        3 => tool_label_style().add_modifier(Modifier::BOLD),
        2 => Style::default()
            .fg(accent_color())
            .add_modifier(Modifier::BOLD),
        1 => Style::default()
            .fg(accent_color())
            .add_modifier(Modifier::BOLD),
        _ => muted(),
    }
}

fn light_sweep_spans(
    text: &str,
    animation_tick: u64,
    style_patch: impl Fn(Style) -> Style,
) -> Vec<Span<'static>> {
    let chars = text.chars().collect::<Vec<_>>();
    if chars.is_empty() {
        return Vec::new();
    }

    let char_count = chars.len();
    chars
        .into_iter()
        .enumerate()
        .map(|(index, ch)| {
            let distance =
                animation::light_sweep_distance(index, char_count, animation_tick).unwrap_or(0);
            let style = match distance {
                0 => style_patch(tool_label_style().add_modifier(Modifier::BOLD)),
                1..=2 => style_patch(Style::default().fg(accent_color())),
                3..=4 => style_patch(message_style(ChatRole::Tool)),
                _ => style_patch(tool_group_meta_style()),
            };
            Span::styled(ch.to_string(), style)
        })
        .collect()
}

fn tool_display_name(name: &str) -> &str {
    match name {
        "file.read" => "read",
        "file.search" => "search",
        "fs.list" => "list",
        "terminal.exec" => "terminal",
        "file.edit" => "edit",
        "file.patch" => "patch",
        "web.fetch" => "fetch",
        "web.search" => "web",
        "task.update" => "status",
        other => other,
    }
}

fn meaningful_tool_output_lines(run: &ToolRun) -> Vec<String> {
    run.detail
        .lines()
        // trim_end only: leading whitespace is meaningful in diff output.
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty() && line.trim() != "done")
        .map(ToString::to_string)
        .collect()
}

/// True when this run's detail carries a display diff from file.edit/file.patch.
fn tool_run_has_diff(run: &ToolRun) -> bool {
    matches!(run.name.as_str(), "file.edit" | "file.patch")
}

/// Convert a detail line containing ANSI escape codes into styled spans.
/// Unstyled segments fall back to the tool body style so colored fragments
/// (e.g. cargo's red `error`) sit inside otherwise-muted output.
fn ansi_detail_spans(line: &str, fallback: Style) -> Vec<Span<'static>> {
    use ansi_to_tui::IntoText;

    let Ok(text) = line.into_text() else {
        return vec![Span::styled(line.replace('\u{1b}', "␛"), fallback)];
    };
    let Some(parsed) = text.lines.into_iter().next() else {
        return Vec::new();
    };
    parsed
        .spans
        .into_iter()
        .map(|span| {
            // Reset/uncolored segments take the tool body style; ansi-to-tui
            // encodes SGR reset as explicit Color::Reset rather than default.
            let unstyled = match span.style.fg {
                None | Some(Color::Reset) => true,
                Some(_) => false,
            };
            let style = if unstyled { fallback } else { span.style };
            Span::styled(span.content.into_owned(), style)
        })
        .collect()
}

fn diff_line_style(line: &str) -> Option<Style> {
    let trimmed = line.trim_start();
    if trimmed.contains("verify:") && trimmed.contains("FAILED") {
        return Some(error_style());
    }
    if trimmed.starts_with("+") {
        Some(Style::default().fg(palette().success))
    } else if trimmed.starts_with("-") {
        Some(Style::default().fg(palette().error))
    } else if trimmed.starts_with("@@") {
        Some(muted().add_modifier(Modifier::DIM))
    } else {
        None
    }
}

fn tool_output_failed(output: &str) -> bool {
    if output.starts_with("failed") || output.starts_with("error:") {
        return true;
    }

    if let Some(exit) = output.strip_prefix("exit: ") {
        let code = exit.split_whitespace().next().unwrap_or("");
        return code != "0";
    }

    false
}

fn compact_tool_detail(output: &str) -> String {
    if output.trim().is_empty() {
        return "done".to_string();
    }

    let is_raw_terminal = output.starts_with("exit:")
        && output
            .lines()
            .any(|line| matches!(line.trim(), "stdout:" | "stderr:" | "stdout: <empty>"));

    // Edit/patch diffs and pre-summarized terminal output are already compacted
    // upstream and their body is the whole point of the expanded view; keep them whole.
    if !is_raw_terminal
        && (output.starts_with("edited ")
            || output.starts_with("patched ")
            || output.starts_with("exit:"))
    {
        return output.to_string();
    }

    // Raw terminal-format output (background streams): drop section markers.
    if is_raw_terminal
        || output.starts_with("patched files:")
        || output.starts_with("edited files:")
    {
        return output
            .lines()
            .filter(|line| {
                let line = line.trim();
                !line.is_empty()
                    && line != "stdout:"
                    && line != "stderr:"
                    && line != "stdout: <empty>"
            })
            .skip(1)
            .take(40)
            .collect::<Vec<_>>()
            .join("\n")
            .if_empty("done");
    }

    output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .take(40)
        .collect::<Vec<_>>()
        .join("\n")
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

fn single_image_path(text: &str) -> Option<PathBuf> {
    let trimmed = text.trim().trim_matches('"').trim_matches('\'');
    if trimmed.lines().count() != 1 {
        return None;
    }
    let path = PathBuf::from(trimmed);
    if !path.is_file() || image_mime_from_path(&path).is_none() {
        return None;
    }
    Some(path)
}

fn image_mime_from_path(path: &Path) -> Option<&'static str> {
    match path
        .extension()?
        .to_string_lossy()
        .to_ascii_lowercase()
        .as_str()
    {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        _ => None,
    }
}

fn image_extension(mime: &str) -> &'static str {
    match mime {
        "image/jpeg" => "jpg",
        _ => "png",
    }
}

fn sanitize_attachment_name(name: &str, extension: &str) -> String {
    let mut safe = name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>();

    if safe.trim_matches('-').is_empty() {
        safe = format!("image.{extension}");
    }
    if Path::new(&safe).extension().is_none() {
        safe.push('.');
        safe.push_str(extension);
    }
    safe
}

fn attachment_timestamp() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn attachment_label(attachment: &ImageAttachment) -> String {
    format!(
        "{} {}×{} {}",
        truncate(&attachment.name, 24),
        attachment.width,
        attachment.height,
        human_bytes(attachment.size_bytes)
    )
}

/// Prompt seeded into the composer by /review. Never auto-sent: the user
/// can trim scope or add focus areas before pressing enter.
const REVIEW_PROMPT_TEMPLATE: &str = "\
Review the pending changes in this workspace.

1. Run `git status`, then `git diff` and `git diff --staged`, to see every pending change.
2. Review for correctness bugs first: logic errors, broken edge cases, races, and regressions.
3. Only then look for simplifications: dead code, duplication, and needless complexity.
4. Verify every claim by reading the surrounding code before reporting it — no guesses.
5. Report each finding as file:line with a one-sentence explanation, most severe first.";

/// True when the workspace is inside a git repo with pending changes
/// (staged, unstaged, or untracked — `git status --porcelain` shows all
/// three). The default probe behind /review's App-injectable check.
fn workspace_has_reviewable_diff(workspace: &Path) -> bool {
    let inside_repo = std::process::Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(workspace)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false);
    if !inside_repo {
        return false;
    }
    std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(workspace)
        .output()
        .map(|output| output.status.success() && !output.stdout.is_empty())
        .unwrap_or(false)
}

/// Single-line picker preview: whitespace (including newlines) collapsed,
/// then char-truncated.
fn message_one_liner(content: &str, max_chars: usize) -> String {
    let flat = content.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate(&flat, max_chars)
}

fn truncate(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let truncated = chars.by_ref().take(max_chars).collect::<String>();

    if chars.next().is_some() {
        format!("{truncated}...")
    } else {
        truncated
    }
}

/// Prompt excerpt stored in checkpoint manifests: first line, ≤80 chars.
fn excerpt_for_checkpoint(task: &str) -> String {
    task.lines()
        .next()
        .unwrap_or("")
        .trim()
        .chars()
        .take(80)
        .collect()
}

/// Compact "3m ago"-style label for checkpoint rows.
fn time_ago_ms(created_at_ms: u64) -> String {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let seconds = now_ms.saturating_sub(created_at_ms) / 1000;
    if seconds < 60 {
        "now".to_string()
    } else if seconds < 3_600 {
        format!("{}m ago", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h ago", seconds / 3_600)
    } else {
        format!("{}d ago", seconds / 86_400)
    }
}

fn tool_summary(summary: &str) -> String {
    let summary = summary.trim();
    if summary.is_empty() {
        "tool call".to_string()
    } else {
        compact_one_line(summary, 160)
    }
}

fn compact_one_line(value: &str, max_chars: usize) -> String {
    truncate(
        &value.split_whitespace().collect::<Vec<_>>().join(" "),
        max_chars,
    )
}

fn clean_model_error(error: &str) -> String {
    let compacted = compact_one_line(error, 320);
    let normalized = compacted.replace(['_', '-'], "").to_ascii_lowercase();

    if normalized.contains("serverisoverloaded") || compacted.contains("currently overloaded") {
        return "model overloaded: Our servers are currently overloaded. Please try again later."
            .to_string();
    }

    if compacted.starts_with("model ") && !compacted.contains("{\"response\"") {
        return compacted;
    }

    if let Some(message) = extract_json_error_message(error) {
        return format!("model failed: {}", compact_one_line(&message, 220));
    }

    format!("model failed: {compacted}")
}

fn model_error_status(error: &str) -> &'static str {
    if error.to_ascii_lowercase().contains("overloaded") {
        "model overloaded"
    } else {
        "model failed"
    }
}

fn extract_json_error_message(error: &str) -> Option<String> {
    let message_key = "\"message\":\"";
    let start = error.find(message_key)? + message_key.len();
    let rest = &error[start..];
    let end = rest.find('"')?;
    Some(rest[..end].replace("\\n", " ").replace("\\\"", "\""))
}

fn abbreviate_home(path: &str) -> String {
    let Some(home) = env::var_os("HOME") else {
        return path.to_string();
    };
    let home = home.to_string_lossy();

    path.strip_prefix(home.as_ref())
        .map(|rest| format!("~{rest}"))
        .unwrap_or_else(|| path.to_string())
}

fn app_bg() -> Color {
    Color::Reset
}

fn surface() -> Color {
    Color::Reset
}

fn text() -> Color {
    palette().text
}

fn accent_color() -> Color {
    palette().accent
}

fn muted() -> Style {
    Style::default().fg(palette().muted)
}

fn accent() -> Style {
    Style::default().fg(accent_color())
}

fn value_style() -> Style {
    Style::default().fg(text())
}

fn separator_style() -> Style {
    Style::default().fg(palette().separator)
}

fn prompt_style() -> Style {
    Style::default()
        .fg(palette().prompt)
        .add_modifier(Modifier::BOLD)
}

fn attachment_preview_border_style() -> Style {
    separator_style()
}

fn attachment_preview_title_style() -> Style {
    success_style()
}

fn attachment_preview_meta_style() -> Style {
    muted()
}

fn tool_label_style() -> Style {
    Style::default()
        .fg(palette().tool)
        .add_modifier(Modifier::BOLD)
}

/// Marker for finished tool calls — small bullet in the theme's tool accent
/// (failures override with the error color).
const TOOL_MARKER: &str = "•";

fn tool_marker_style() -> Style {
    Style::default()
        .fg(palette().tool)
        .add_modifier(Modifier::BOLD)
}

fn tool_group_label_style() -> Style {
    Style::default()
        .fg(palette().tool)
        .add_modifier(Modifier::BOLD)
}

fn tool_group_meta_style() -> Style {
    muted()
}

fn success_style() -> Style {
    Style::default()
        .fg(palette().success)
        .add_modifier(Modifier::BOLD)
}

fn error_style() -> Style {
    Style::default()
        .fg(palette().error)
        .add_modifier(Modifier::BOLD)
}

fn error_preview_style() -> Style {
    Style::default().fg(palette().error)
}

fn tool_output_style(state: ToolRunState) -> Style {
    match state {
        ToolRunState::Failed => error_preview_style(),
        _ => message_style(ChatRole::Tool),
    }
}

fn code_border_style() -> Style {
    separator_style()
}

fn code_block_style() -> Style {
    Style::default().fg(palette().code_fg).bg(palette().code_bg)
}

fn inline_code_style() -> Style {
    Style::default()
        .fg(palette().inline_code_fg)
        .bg(palette().inline_code_bg)
}

fn heading_style(level: usize) -> Style {
    let color = if level <= 2 {
        accent_color()
    } else {
        palette().info
    };

    Style::default().fg(color).add_modifier(Modifier::BOLD)
}

fn quote_border_style() -> Style {
    Style::default().fg(palette().quote)
}

fn quote_style() -> Style {
    Style::default()
        .fg(palette().quote)
        .add_modifier(Modifier::ITALIC)
}

fn list_marker_style() -> Style {
    Style::default()
        .fg(palette().prompt)
        .add_modifier(Modifier::BOLD)
}

fn link_style() -> Style {
    Style::default()
        .fg(palette().info)
        .add_modifier(Modifier::UNDERLINED)
}

fn user_message_background_style() -> Style {
    Style::default().bg(palette().user_bg)
}

fn user_message_style() -> Style {
    user_message_background_style().fg(palette().text)
}

fn user_message_prompt_style() -> Style {
    user_message_background_style()
        .fg(palette().prompt)
        .add_modifier(Modifier::BOLD)
}

fn theme_preview_swatch_line(
    label: &'static str,
    color: Color,
    sample: &'static str,
) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<8}"), muted()),
        Span::styled("███", Style::default().fg(color)),
        Span::styled("  ", muted()),
        Span::styled(
            sample,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
    ])
}

fn theme_preview_lines(theme: ThemeKind) -> Vec<Line<'static>> {
    let palette = theme.palette();
    vec![
        Line::from(vec![
            Span::styled("swatches ", muted()),
            Span::styled("██", Style::default().fg(palette.accent)),
            Span::styled(" ", muted()),
            Span::styled("██", Style::default().fg(palette.prompt)),
            Span::styled(" ", muted()),
            Span::styled("██", Style::default().fg(palette.tool)),
            Span::styled(" ", muted()),
            Span::styled("██", Style::default().fg(palette.success)),
            Span::styled(" ", muted()),
            Span::styled("██", Style::default().fg(palette.error)),
        ]),
        theme_preview_swatch_line("accent", palette.accent, "selection / focus"),
        theme_preview_swatch_line("prompt", palette.prompt, "typed prompts"),
        theme_preview_swatch_line("tool", palette.tool, "tool calls"),
        theme_preview_swatch_line("success", palette.success, "completed work"),
        theme_preview_swatch_line("error", palette.error, "failures"),
        Line::from(vec![
            Span::styled("sample  ", muted()),
            Span::styled(
                "› ask Medusa to inspect code",
                Style::default().fg(palette.prompt),
            ),
        ]),
        Line::from(vec![
            Span::styled("answer  ", muted()),
            Span::styled(
                "Markdown, tools, and code render with this palette.",
                Style::default().fg(palette.text),
            ),
        ]),
        Line::from(vec![
            Span::styled("user    ", muted()),
            Span::styled(
                " message surface ",
                Style::default().fg(palette.text).bg(palette.user_bg),
            ),
            Span::styled("  ", muted()),
            Span::styled(
                "inline code",
                Style::default()
                    .fg(palette.inline_code_fg)
                    .bg(palette.inline_code_bg),
            ),
        ]),
    ]
}

fn model_detail_lines(selected: &str, active: &str) -> Vec<Line<'static>> {
    let is_active = selected == active;
    let (display_name, description) = model_display(selected);
    let mut lines = vec![Line::from(vec![
        Span::styled(
            display_name.clone(),
            prompt_style().add_modifier(Modifier::BOLD),
        ),
        Span::styled("  ", muted()),
        Span::styled(
            if is_active { "active" } else { "ready to save" },
            if is_active { success_style() } else { muted() },
        ),
    ])];
    if display_name != selected {
        lines.push(Line::from(Span::styled(selected.to_string(), muted())));
    }
    lines.push(Line::from(""));
    // Backend-provided description, when the Codex model cache has one.
    if let Some(description) = description {
        lines.push(Line::from(Span::styled(description, value_style())));
        lines.push(Line::from(""));
    }
    lines.extend([
        Line::from(Span::styled(
            "The selected model is used for new Medusa turns. Active streams keep the model they started with.",
            value_style(),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("backend  ", muted()),
            Span::styled(model_backend_hint(selected), value_style()),
        ]),
        Line::from(vec![
            Span::styled("config  ", muted()),
            Span::styled(".medusa/settings.json", value_style()),
        ]),
        Line::from(vec![
            Span::styled("override  ", muted()),
            Span::styled("MEDUSA_MODEL", value_style()),
            Span::styled(" wins for one-off launches", muted()),
        ]),
        Line::from(vec![
            Span::styled("provider  ", muted()),
            Span::styled("MEDUSA_PROVIDER", value_style()),
            Span::styled(" can force codex, deepseek, or openai-compatible", muted()),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("custom  ", muted()),
            Span::styled("/model <provider-model-id>", prompt_style()),
        ]),
        Line::from(vec![
            Span::styled("effort  ", muted()),
            Span::styled("/reasoning", prompt_style()),
            Span::styled(" sets thinking depth", muted()),
        ]),
    ]);
    lines
}

fn reasoning_detail_lines(model: &str, selected: &str, active: &str) -> Vec<Line<'static>> {
    let is_active = selected == active;
    let mut lines = vec![Line::from(vec![
        Span::styled(
            selected.to_string(),
            prompt_style().add_modifier(Modifier::BOLD),
        ),
        Span::styled("  ", muted()),
        Span::styled(
            if is_active { "active" } else { "ready to save" },
            if is_active { success_style() } else { muted() },
        ),
    ])];
    lines.push(Line::from(""));
    // Backend-provided description for this effort, when the model cache has one.
    if let Some(description) = reasoning_description(model, selected) {
        lines.push(Line::from(Span::styled(description, value_style())));
        lines.push(Line::from(""));
    }
    lines.extend([
        Line::from(Span::styled(
            "Applies to new turns; active streams keep the effort they started with.",
            value_style(),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("override  ", muted()),
            Span::styled("MEDUSA_REASONING_EFFORT", value_style()),
            Span::styled(" wins for one-off launches", muted()),
        ]),
        Line::from(vec![
            Span::styled("none  ", muted()),
            Span::styled("disables reasoning entirely", value_style()),
        ]),
    ]);
    lines
}

fn model_backend_hint(model: &str) -> &'static str {
    let normalized = model.trim().to_ascii_lowercase();
    if normalized.starts_with("deepseek") {
        "deepseek · requires DEEPSEEK_API_KEY"
    } else if normalized.starts_with("gpt-") {
        "codex · uses Codex OAuth"
    } else {
        "inferred from MEDUSA_PROVIDER"
    }
}

fn permission_detail_lines(selected: PermissionMode, active: PermissionMode) -> Vec<Line<'static>> {
    let is_active = selected == active;
    let mut lines = vec![
        Line::from(vec![
            Span::styled(selected.label(), prompt_style()),
            Span::styled("  ", muted()),
            Span::styled(
                if is_active { "active" } else { "ready to save" },
                if is_active { success_style() } else { muted() },
            ),
        ]),
        Line::from(""),
        Line::from(Span::styled(selected.description(), value_style())),
        Line::from(""),
    ];

    match selected {
        PermissionMode::Open => {
            lines.extend([
                Line::from(vec![
                    Span::styled("terminal  ", muted()),
                    Span::styled(
                        "allow unless explicitly denied by future custom config",
                        value_style(),
                    ),
                ]),
                Line::from(vec![
                    Span::styled("edits     ", muted()),
                    Span::styled("allow within workspace boundary", value_style()),
                ]),
            ]);
        }
        PermissionMode::Ask => {
            lines.extend([
                Line::from(vec![
                    Span::styled("terminal  ", muted()),
                    Span::styled(
                        "safe reads run freely; other commands pause for approval",
                        value_style(),
                    ),
                ]),
                Line::from(vec![
                    Span::styled("edits     ", muted()),
                    Span::styled("file edits and patches pause for approval", value_style()),
                ]),
                Line::from(vec![
                    Span::styled("grants    ", muted()),
                    Span::styled(
                        "'always allow' persists to .medusa/permissions.json",
                        value_style(),
                    ),
                ]),
            ]);
        }
        PermissionMode::Guarded => {
            lines.extend([
                Line::from(vec![
                    Span::styled("terminal  ", muted()),
                    Span::styled("deny common destructive fragments", value_style()),
                ]),
                Line::from(vec![
                    Span::styled("edits     ", muted()),
                    Span::styled("deny .git and Medusa session internals", value_style()),
                ]),
            ]);
        }
        PermissionMode::Readonly => {
            lines.extend([
                Line::from(vec![
                    Span::styled("terminal  ", muted()),
                    Span::styled("allow common inspection commands", value_style()),
                ]),
                Line::from(vec![
                    Span::styled("edits     ", muted()),
                    Span::styled("block file_edit and file_patch", value_style()),
                ]),
            ]);
        }
    }

    lines.extend([
        Line::from(""),
        Line::from(vec![
            Span::styled("config  ", muted()),
            Span::styled(".medusa/permissions.json", value_style()),
        ]),
    ]);
    lines
}

fn command_has_shell_tokens(command: &str) -> bool {
    [
        "\n", "\r", ";", "&&", "||", "|", "&", ">", "<", "`", "$(", "${",
    ]
    .iter()
    .any(|token| command.contains(token))
}

/// Hard-wrap a string to at most `width` columns per line (character-based,
/// no word splitting when a break point exists). Caps the number of lines so a
/// pathological command can't grow the prompt off-screen.
fn wrap_str(text: &str, width: usize) -> Vec<String> {
    const MAX_LINES: usize = 8;
    let width = width.max(8);
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut count = 0usize;
    for ch in text.chars() {
        if count >= width {
            lines.push(std::mem::take(&mut current));
            count = 0;
            if lines.len() == MAX_LINES {
                current.push('…');
                break;
            }
        }
        current.push(ch);
        count += 1;
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// Strip leading `VAR=value` assignments so grant matching sees the program.
fn strip_env_assignments(command: &str) -> &str {
    let mut rest = command.trim_start();
    loop {
        let word_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let word = &rest[..word_end];
        if word.contains('=') && !word.starts_with('-') && !word.is_empty() {
            rest = rest[word_end..].trim_start();
        } else {
            return rest;
        }
    }
}

fn command_matches_grant(command: &str, prefix: &str) -> bool {
    command == prefix
        || command
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with(char::is_whitespace))
}

/// A session edit grant ending in `/` covers a directory subtree; otherwise it
/// is an exact file path (so `Cargo.toml` never grants `Cargo.toml.bak`).
fn edit_grant_matches(grants: &[String], path: &str) -> bool {
    grants.iter().any(|grant| {
        if grant.ends_with('/') {
            path.starts_with(grant.as_str())
        } else {
            path == grant
        }
    })
}

/// Derive the allow-prefix persisted by "always allow": program name, plus
/// the subcommand for multi-command tools, never for compound shell strings.
fn derive_terminal_grant_prefix(command: &str) -> Option<String> {
    let command = command.trim();
    if command.is_empty() || command_has_shell_tokens(command) {
        return None;
    }

    let mut words = command
        .split_whitespace()
        .skip_while(|word| word.contains('=') && !word.starts_with('-'));
    let program = words.next()?;

    // Interpreters and shells take arbitrary code as arguments, so a prefix
    // grant on them ("always allow bash") is a blanket execution grant. These
    // only ever get allow-once, never a persisted/session prefix.
    const INTERPRETER_PROGRAMS: &[&str] = &[
        "sh",
        "bash",
        "zsh",
        "fish",
        "dash",
        "ksh",
        "python",
        "python3",
        "python2",
        "node",
        "deno",
        "ruby",
        "perl",
        "php",
        "lua",
        "Rscript",
        "osascript",
        "env",
        "eval",
        "exec",
        "xargs",
        "nohup",
        "time",
        "sudo",
        "doas",
        "ssh",
        "docker",
        "kubectl",
    ];
    if INTERPRETER_PROGRAMS.contains(&program) {
        return None;
    }

    const SUBCOMMAND_PROGRAMS: &[&str] = &[
        "git", "cargo", "npm", "pnpm", "yarn", "bun", "make", "go", "pip", "pip3", "uv", "just",
    ];
    if !SUBCOMMAND_PROGRAMS.contains(&program) {
        return Some(program.to_string());
    }

    let subcommand = words.next().filter(|word| !word.starts_with('-'));
    match subcommand {
        Some("run") => {
            let script = words.next().filter(|word| !word.starts_with('-'));
            match script {
                Some(script) => Some(format!("{program} run {script}")),
                None => Some(format!("{program} run")),
            }
        }
        Some(sub) => Some(format!("{program} {sub}")),
        None => Some(program.to_string()),
    }
}

fn permission_context_text(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Open => {
            "Medusa permission mode: open. Normal workspace inspection, terminal commands, and file mutations are available subject to workspace boundaries."
        }
        PermissionMode::Guarded => {
            "Medusa permission mode: guarded. Workspace inspection is available. Terminal commands and file mutations are allowed unless blocked by Medusa's guarded permission policy. Mention guarded mode when a command or edit is blocked."
        }
        PermissionMode::Ask => {
            "Medusa permission mode: ask. Safe inspection commands run freely; mutating terminal commands and file edits pause for the user's interactive approval. If a tool result says it was denied by the user, respect the decision — do not retry the same operation; adjust the approach or ask the user."
        }
        PermissionMode::Readonly => {
            "Medusa permission mode: readonly. Reading, listing, searching, and safe inspection commands are allowed. File mutation tools are unavailable for this turn, and write-like shell commands are blocked by policy. If the user asks for changes, explain that this session is in readonly mode and offer a plan or ask them to switch permissions."
        }
    }
}

struct SessionStateRuntime<'a> {
    workspace: &'a str,
    model: &'a str,
    permission_mode: PermissionMode,
    status: &'a str,
    workflows: &'a [WorkflowRunView],
    active_workflows: usize,
    background_jobs: &'a BTreeMap<String, BackgroundJobView>,
}

fn transcript_conversation_message(item: &TranscriptItem) -> Option<ConversationMessage> {
    match item {
        TranscriptItem::Message(message) => match message.role {
            ChatRole::User => Some(ConversationMessage {
                role: "user".to_string(),
                content: message.content.clone(),
                attachments: message
                    .attachments
                    .iter()
                    .map(|attachment| ConversationAttachment {
                        mime: attachment.mime.clone(),
                        path: attachment.path.clone(),
                    })
                    .collect(),
            }),
            ChatRole::Assistant if !message.content.trim().is_empty() => {
                Some(ConversationMessage {
                    role: "assistant".to_string(),
                    content: message.content.clone(),
                    attachments: Vec::new(),
                })
            }
            // The interruption note re-enters model history so a resumed
            // conversation knows the previous turn was cut short.
            ChatRole::System if message.content == TURN_INTERRUPTED_NOTE => {
                Some(ConversationMessage {
                    role: "system".to_string(),
                    content: message.content.clone(),
                    attachments: Vec::new(),
                })
            }
            _ => None,
        },
        _ => None,
    }
}

fn session_state_context_text(
    transcript: &[TranscriptItem],
    recent_message_count: usize,
    runtime: SessionStateRuntime<'_>,
) -> String {
    let conversation_total = transcript
        .iter()
        .filter(|item| transcript_conversation_message(item).is_some())
        .count();
    let tool_total = transcript
        .iter()
        .filter(|item| matches!(item, TranscriptItem::Tool(_)))
        .count();
    let reasoning_total = transcript
        .iter()
        .filter(|item| matches!(item, TranscriptItem::Reasoning(_)))
        .count();
    let omitted = conversation_total.saturating_sub(recent_message_count);
    let running_jobs = runtime
        .background_jobs
        .values()
        .filter(|job| job.state == ToolRunState::Running)
        .count();

    let mut lines = vec![
        "Medusa rolling session state and semantic memory.".to_string(),
        format!(
            "workspace: {} · model: {} · permissions: {} · status: {}",
            runtime.workspace,
            runtime.model,
            runtime.permission_mode.name(),
            compact_one_line(runtime.status, 120)
        ),
        format!(
            "context window: {recent_message_count}/{conversation_total} recent conversation messages retained; {omitted} older messages summarized; {tool_total} tool rows; {reasoning_total} reasoning traces not replayed verbatim."
        ),
    ];

    append_context_section(
        &mut lines,
        "semantic memory",
        semantic_memory_lines(transcript),
    );
    append_context_section(
        &mut lines,
        "recent user intents",
        recent_user_intents(transcript),
    );
    append_context_section(
        &mut lines,
        "recent assistant outcomes",
        recent_assistant_outcomes(transcript),
    );
    append_context_section(
        &mut lines,
        "session/system notes",
        recent_system_notes(transcript),
    );
    append_context_section(
        &mut lines,
        "tool history",
        recent_tool_summaries(transcript),
    );
    append_context_section(
        &mut lines,
        "changed or referenced files",
        file_mentions(transcript),
    );
    append_context_section(
        &mut lines,
        "workflow state",
        workflow_state_lines(runtime.workflows, runtime.active_workflows),
    );
    append_context_section(
        &mut lines,
        "decision state",
        decision_state_lines(transcript),
    );

    if !runtime.background_jobs.is_empty() {
        lines.push("background jobs:".to_string());
        lines.push(format!(
            "- {} total; {running_jobs} running",
            runtime.background_jobs.len()
        ));
        for job in runtime.background_jobs.values().rev().take(4) {
            lines.push(format!(
                "- {} {:?}: {}",
                compact_one_line(&job.id, 24),
                job.state,
                compact_one_line(&job.command, 100)
            ));
        }
    }

    lines.push(
        "Use semantic memory as durable session state. Use recent messages for exact wording. Inspect files with fs_list/file_search/file_read before relying on exact workspace state or editing.".to_string(),
    );
    lines.join("\n")
}

fn append_context_section(lines: &mut Vec<String>, title: &str, items: Vec<String>) {
    if items.is_empty() {
        return;
    }
    lines.push(format!("{title}:"));
    for item in items {
        lines.push(format!("- {item}"));
    }
}

fn recent_user_intents(transcript: &[TranscriptItem]) -> Vec<String> {
    transcript
        .iter()
        .rev()
        .filter_map(|item| match item {
            TranscriptItem::Message(message)
                if message.role == ChatRole::User && !message.content.trim().is_empty() =>
            {
                Some(compact_one_line(&message.content, 180))
            }
            _ => None,
        })
        .take(SESSION_STATE_MAX_INTENTS)
        .collect()
}

fn recent_assistant_outcomes(transcript: &[TranscriptItem]) -> Vec<String> {
    transcript
        .iter()
        .rev()
        .filter_map(|item| match item {
            TranscriptItem::Message(message)
                if message.role == ChatRole::Assistant && !message.content.trim().is_empty() =>
            {
                Some(compact_one_line(&message.content, 220))
            }
            _ => None,
        })
        .take(SESSION_STATE_MAX_OUTCOMES)
        .collect()
}

fn recent_system_notes(transcript: &[TranscriptItem]) -> Vec<String> {
    transcript
        .iter()
        .rev()
        .filter_map(|item| match item {
            TranscriptItem::Message(message)
                if message.role == ChatRole::System && !message.content.trim().is_empty() =>
            {
                Some(compact_one_line(&message.content, 180))
            }
            _ => None,
        })
        .take(SESSION_STATE_MAX_SYSTEM_NOTES)
        .collect()
}

#[derive(Default)]
struct SemanticMemory {
    preferences: Vec<String>,
    decisions: Vec<String>,
    issues: Vec<String>,
    validations: Vec<String>,
    outcomes: Vec<String>,
}

impl SemanticMemory {
    fn is_full(&self) -> bool {
        self.preferences.len() >= SESSION_MEMORY_MAX_PER_KIND
            && self.decisions.len() >= SESSION_MEMORY_MAX_PER_KIND
            && self.issues.len() >= SESSION_MEMORY_MAX_PER_KIND
            && self.validations.len() >= SESSION_MEMORY_MAX_PER_KIND
            && self.outcomes.len() >= SESSION_MEMORY_MAX_PER_KIND
    }

    fn lines(self) -> Vec<String> {
        let mut lines = Vec::new();
        append_memory_kind(&mut lines, "preference", self.preferences);
        append_memory_kind(&mut lines, "decision", self.decisions);
        append_memory_kind(&mut lines, "issue", self.issues);
        append_memory_kind(&mut lines, "validation", self.validations);
        append_memory_kind(&mut lines, "outcome", self.outcomes);
        lines
    }
}

fn append_memory_kind(lines: &mut Vec<String>, label: &str, items: Vec<String>) {
    for item in items {
        lines.push(format!("{label}: {item}"));
    }
}

fn semantic_memory_lines(transcript: &[TranscriptItem]) -> Vec<String> {
    let mut memory = SemanticMemory::default();

    for item in transcript.iter().rev() {
        match item {
            TranscriptItem::Message(message) => {
                collect_message_memory(message, &mut memory);
            }
            TranscriptItem::Tool(run) => collect_tool_memory(run, &mut memory),
            TranscriptItem::Workflow(workflow) => collect_workflow_memory(workflow, &mut memory),
            TranscriptItem::Plan(plan) => collect_plan_memory(plan, &mut memory),
            TranscriptItem::Decision(decision) => collect_decision_memory(decision, &mut memory),
            TranscriptItem::Reasoning(_) => {}
        }

        if memory.is_full() {
            break;
        }
    }

    memory.lines()
}

fn collect_message_memory(message: &ChatMessage, memory: &mut SemanticMemory) {
    let text = compact_one_line(&message.content, 220);
    if text.trim().is_empty() {
        return;
    }

    let lower = text.to_ascii_lowercase();
    match message.role {
        ChatRole::User => {
            if contains_any(
                &lower,
                &[
                    "i prefer",
                    "prefer ",
                    "i like",
                    "i want",
                    "i don't want",
                    "i dont want",
                    "do not",
                    "don't",
                    "dont ",
                    "never ",
                    "always ",
                    "must ",
                    "has to",
                    "should ",
                    "shouldn't",
                    "shouldnt",
                    "keep it",
                    "we need",
                    "we don't",
                    "we dont",
                ],
            ) {
                push_memory(&mut memory.preferences, text.clone());
            }

            if contains_any(
                &lower,
                &[
                    "we decided",
                    "decided",
                    "let's use",
                    "lets use",
                    "use ratatui",
                    "keep it medusa",
                    "we will",
                    "we'll",
                    "we wont",
                    "we won't",
                ],
            ) {
                push_memory(&mut memory.decisions, text.clone());
            }

            if contains_any(
                &lower,
                &[
                    "broken", "error", "failed", "failing", "doesn't", "doesnt", "can't", "cant ",
                    "stuck", "lag", "bad", "fix ",
                ],
            ) {
                push_memory(&mut memory.issues, text);
            }
        }
        ChatRole::Assistant => {
            if contains_any(
                &lower,
                &[
                    "implemented",
                    "added",
                    "changed",
                    "fixed",
                    "wired",
                    "updated",
                    "validation",
                    "passed",
                    "green",
                ],
            ) {
                push_memory(&mut memory.outcomes, text.clone());
            }

            if contains_any(
                &lower,
                &[
                    "cargo test",
                    "cargo check",
                    "passed",
                    "failed",
                    "validation",
                    "tests",
                ],
            ) {
                push_memory(&mut memory.validations, text);
            }
        }
        ChatRole::System | ChatRole::Tool => {}
    }
}

fn collect_tool_memory(run: &ToolRun, memory: &mut SemanticMemory) {
    let summary = tool_summary(&run.summary);
    let detail = compact_one_line(&run.detail, 180);
    let combined = compact_one_line(&format!("{summary} {detail}"), 220);

    if run.state == ToolRunState::Failed {
        push_memory(
            &mut memory.issues,
            format!("{} failed: {}", run.name, combined),
        );
        return;
    }

    if run.name.contains("patch") || run.name.contains("edit") {
        push_memory(
            &mut memory.outcomes,
            format!("{} changed workspace: {}", run.name, combined),
        );
    }

    let lower = combined.to_ascii_lowercase();
    if contains_any(
        &lower,
        &["cargo test", "cargo check", "passed", "finished", "ok"],
    ) {
        push_memory(
            &mut memory.validations,
            format!("{} succeeded: {}", run.name, combined),
        );
    }
}

fn collect_plan_memory(plan: &PlanView, memory: &mut SemanticMemory) {
    if plan.items.is_empty() {
        return;
    }

    let progress = plan_progress(plan);
    let title = if plan.summary.trim().is_empty() {
        "current plan".to_string()
    } else {
        compact_one_line(&plan.summary, 100)
    };
    push_memory(
        &mut memory.decisions,
        format!(
            "plan: {title} · {} steps · {} done · {} blocked",
            plan.items.len(),
            progress.done,
            progress.blocked
        ),
    );

    for item in plan
        .items
        .iter()
        .filter(|item| item.status == PlanItemStatus::Blocked)
    {
        push_memory(
            &mut memory.issues,
            format!("blocked plan step: {}", compact_one_line(&item.text, 140)),
        );
    }
}

fn collect_decision_memory(decision: &DecisionView, memory: &mut SemanticMemory) {
    let title = if decision.title.trim().is_empty() {
        "planning decision".to_string()
    } else {
        compact_one_line(&decision.title, 100)
    };

    if decision.answered {
        let answer = decision
            .answer
            .as_deref()
            .map(|answer| compact_one_line(answer, 140))
            .unwrap_or_else(|| "answered".to_string());
        push_memory(
            &mut memory.decisions,
            format!("answered decision: {title} · {answer}"),
        );
    } else {
        push_memory(
            &mut memory.issues,
            format!(
                "pending decision: {title} · {} question(s)",
                decision.questions.len()
            ),
        );
    }
}

fn collect_workflow_memory(workflow: &WorkflowRunView, memory: &mut SemanticMemory) {
    let title = compact_one_line(&workflow.title, 80);
    let summary = compact_one_line(
        if workflow.summary.trim().is_empty() {
            &workflow.task
        } else {
            &workflow.summary
        },
        180,
    );

    match workflow.status {
        WorkflowViewState::Succeeded => {
            push_memory(
                &mut memory.outcomes,
                format!("workflow succeeded: {title} · {summary}"),
            );
        }
        WorkflowViewState::PartiallySucceeded => {
            push_memory(
                &mut memory.outcomes,
                format!("workflow partially completed: {title} · {summary}"),
            );
            push_memory(
                &mut memory.issues,
                format!("workflow had failed subagents: {title} · {summary}"),
            );
        }
        WorkflowViewState::Failed => {
            push_memory(
                &mut memory.issues,
                format!("workflow failed: {title} · {summary}"),
            );
        }
        WorkflowViewState::Running | WorkflowViewState::Pending => {}
    }
}

fn push_memory(items: &mut Vec<String>, item: String) {
    if items.len() >= SESSION_MEMORY_MAX_PER_KIND || item.trim().is_empty() {
        return;
    }
    if !items.iter().any(|existing| existing == &item) {
        items.push(item);
    }
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| haystack.contains(needle))
}

fn recent_tool_summaries(transcript: &[TranscriptItem]) -> Vec<String> {
    transcript
        .iter()
        .rev()
        .filter_map(|item| match item {
            TranscriptItem::Tool(run) => Some(format!(
                "{} {}: {}{}",
                run.name,
                session_tool_state_label(run.state),
                compact_one_line(&tool_summary(&run.summary), 140),
                tool_detail_suffix(run)
            )),
            _ => None,
        })
        .take(SESSION_STATE_MAX_TOOLS)
        .collect()
}

fn session_tool_state_label(state: ToolRunState) -> &'static str {
    match state {
        ToolRunState::Running => "running",
        ToolRunState::Succeeded => "succeeded",
        ToolRunState::Failed => "failed",
    }
}

fn tool_detail_suffix(run: &ToolRun) -> String {
    if run.detail.trim().is_empty() {
        return String::new();
    }
    format!(" · {}", compact_one_line(&run.detail, 160))
}

fn file_mentions(transcript: &[TranscriptItem]) -> Vec<String> {
    let mut files = Vec::new();
    for item in transcript.iter().rev() {
        if let TranscriptItem::Tool(run) = item {
            collect_file_mentions_from_text(&run.summary, &mut files);
            collect_file_mentions_from_text(&run.detail, &mut files);
        }
        if files.len() >= SESSION_STATE_MAX_FILES {
            break;
        }
    }
    files
}

fn collect_file_mentions_from_text(text: &str, files: &mut Vec<String>) {
    for token in text.split_whitespace() {
        if files.len() >= SESSION_STATE_MAX_FILES {
            return;
        }
        let token = token.trim_matches(|ch: char| {
            matches!(
                ch,
                '`' | '\'' | '"' | ',' | ':' | ';' | ')' | '(' | '[' | ']' | '{' | '}'
            )
        });
        if is_workspace_file_mention(token) && !files.iter().any(|file| file == token) {
            files.push(token.to_string());
        }
    }
}

fn is_workspace_file_mention(token: &str) -> bool {
    if token.is_empty()
        || token.starts_with("http://")
        || token.starts_with("https://")
        || token.starts_with('$')
        || token.contains("://")
        || token.contains(' ')
    {
        return false;
    }

    let has_path_shape = token.contains('/')
        || token.starts_with(".medusa/")
        || token.starts_with("Cargo.")
        || token.starts_with("README")
        || token.starts_with("Makefile");
    let has_file_shape = Path::new(token).extension().is_some()
        || token.ends_with("Makefile")
        || token.ends_with("Dockerfile");
    has_path_shape && has_file_shape
}

fn workflow_state_lines(workflows: &[WorkflowRunView], active_workflows: usize) -> Vec<String> {
    if workflows.is_empty() && active_workflows == 0 {
        return Vec::new();
    }
    let mut lines = vec![format!(
        "{} total; {active_workflows} active",
        workflows.len()
    )];
    lines.extend(workflows.iter().rev().take(6).map(|workflow| {
        format!(
            "{} {:?}: {}",
            compact_one_line(&workflow.title, 80),
            workflow.status,
            compact_one_line(
                if workflow.summary.trim().is_empty() {
                    &workflow.task
                } else {
                    &workflow.summary
                },
                160,
            )
        )
    }));
    lines
}

fn decision_state_lines(transcript: &[TranscriptItem]) -> Vec<String> {
    transcript
        .iter()
        .rev()
        .filter_map(|item| match item {
            TranscriptItem::Decision(decision) => {
                let title = if decision.title.trim().is_empty() {
                    "planning decision".to_string()
                } else {
                    compact_one_line(&decision.title, 100)
                };
                let status = if decision.answered {
                    "answered"
                } else {
                    "waiting"
                };
                let answer = decision
                    .answer
                    .as_deref()
                    .map(|answer| format!(" · answer: {}", compact_one_line(answer, 120)))
                    .unwrap_or_default();
                Some(format!(
                    "{status}: {title} · {} question(s){answer}",
                    decision.questions.len()
                ))
            }
            _ => None,
        })
        .take(4)
        .collect()
}

fn message_style(role: ChatRole) -> Style {
    match role {
        ChatRole::User => user_message_style(),
        ChatRole::Assistant => value_style(),
        ChatRole::Tool => Style::default().fg(palette().tool),
        ChatRole::System => error_preview_style(),
    }
}

fn command_selected_style() -> Style {
    Style::default()
        .fg(palette().selected_fg)
        .bg(palette().selected_bg)
        .add_modifier(Modifier::BOLD)
}

fn activity_selected_style() -> Style {
    Style::default().bg(palette().activity_bg)
}

fn toast_style(kind: ToastKind) -> Style {
    match kind {
        ToastKind::Info => Style::default().fg(palette().info),
        ToastKind::Success => success_style(),
        ToastKind::Warning => prompt_style(),
        ToastKind::Error => error_style(),
    }
}

fn toast_label(kind: ToastKind) -> &'static str {
    match kind {
        ToastKind::Info => "notice",
        ToastKind::Success => "done",
        ToastKind::Warning => "warning",
        ToastKind::Error => "error",
    }
}

fn modal_block(title: &'static str) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(accent_color()))
        .style(Style::default().bg(surface()).fg(text()))
}

fn centered_rect(parent: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(parent.width);
    let height = height.min(parent.height);
    let x = parent.x + parent.width.saturating_sub(width) / 2;
    let y = parent.y + parent.height.saturating_sub(height) / 2;

    Rect::new(x, y, width, height)
}

fn command_palette_rect(parent: Rect, item_count: usize) -> Rect {
    let max_width = parent.width.saturating_sub(4).max(1);
    let width = max_width.min(92);
    let max_height = parent.height.saturating_sub(4).max(3);
    let desired_height = (item_count as u16 + 5).clamp(9, 18);
    let height = desired_height.min(max_height);

    centered_rect(parent, width, height)
}

fn cursor_style() -> Style {
    Style::default().fg(accent_color())
}

fn placeholder_style() -> Style {
    muted().add_modifier(Modifier::ITALIC)
}

#[cfg(test)]
mod tests {
    use super::*;
    use medusa_core::session::{compact_session_id, normalize_session_name, read_session_file};
    use medusa_core::workflow::{SubagentSpec, SubagentToolPolicy};

    fn app() -> App {
        App::with_model_backend(false)
    }

    /// Wrap a raw workflow-event receiver into a `BackgroundWorkflow` for tests
    /// that push directly onto `app.workflow_events`.
    fn background_workflow(app: &App, events: Receiver<WorkflowEvent>) -> BackgroundWorkflow {
        BackgroundWorkflow {
            events,
            checkpoint: app.new_workflow_checkpoint("/workflow test", 0),
            cancel: CancelToken::new(),
        }
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn first_span_fg_containing(rows: &[TranscriptRow], needle: &str) -> Option<Color> {
        rows.iter()
            .flat_map(|row| row.line.spans.iter())
            .find(|span| span.content.contains(needle))
            .and_then(|span| span.style.fg)
    }

    fn wheel_event(kind: MouseEventKind, modifiers: KeyModifiers) -> MouseEvent {
        MouseEvent {
            kind,
            column: 0,
            row: 0,
            modifiers,
        }
    }

    fn scrollback_app(line_count: usize, viewport: Rect) -> App {
        let mut app = app();
        app.transcript = (0..line_count)
            .map(|index| TranscriptItem::Message(ChatMessage::assistant(format!("line {index}"))))
            .collect();
        app.last_chat_viewport = Some(viewport);
        app
    }

    fn image_attachment(id: &str) -> ImageAttachment {
        ImageAttachment {
            id: id.to_string(),
            name: format!("{id}.png"),
            path: PathBuf::from(format!("/tmp/{id}.png")),
            mime: "image/png".to_string(),
            width: 320,
            height: 180,
            size_bytes: 42_000,
        }
    }

    fn temp_workspace() -> PathBuf {
        // pid + atomic counter: unique across parallel test threads and
        // concurrent test processes (a bare timestamp raced in the past).
        static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!("medusa-tui-test-{}-{suffix}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        path.canonicalize().unwrap()
    }

    fn app_in_workspace() -> (App, PathBuf) {
        let workspace = temp_workspace();
        let mut app = app();
        app.tools = ToolRuntime::new(&workspace).unwrap();
        app.model = DirectCodexBackend::new(&workspace).unwrap();
        app.cwd_display = abbreviate_home(&workspace.to_string_lossy());
        (app, workspace)
    }

    #[test]
    fn theme_preference_round_trips_through_workspace_settings() {
        let workspace = temp_workspace();

        save_theme_preference(&workspace, ThemeKind::Gruvbox).unwrap();

        let settings = load_app_settings(&workspace).unwrap();
        assert_eq!(settings.theme(), Some(ThemeKind::Gruvbox));
        assert_eq!(
            ThemeKind::from_workspace_settings(&workspace),
            ThemeKind::Gruvbox
        );
    }

    #[test]
    fn env_theme_overrides_workspace_settings() {
        let workspace = temp_workspace();
        save_theme_preference(&workspace, ThemeKind::Gruvbox).unwrap();

        // Pass the override explicitly instead of mutating process-global env:
        // `set_var("MEDUSA_THEME", …)` races the parallel test harness's
        // getenv-backed readers (UB) and its sibling `from_workspace_settings`
        // callers (flaky logic race).
        assert_eq!(
            ThemeKind::resolve(Some("nord"), &workspace),
            ThemeKind::Nord
        );
        // With no override the persisted workspace setting wins.
        assert_eq!(ThemeKind::resolve(None, &workspace), ThemeKind::Gruvbox);
    }

    #[test]
    fn typing_updates_input() {
        let mut app = app();

        app.handle_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE));

        assert_eq!(app.input, "hi");
        assert_eq!(app.input_cursor, 2);
    }

    #[test]
    fn backspace_edits_input() {
        let mut app = app();

        app.input = "fixx".to_string();
        app.input_cursor = 4;
        app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));

        assert_eq!(app.input, "fix");
        assert_eq!(app.input_cursor, 3);
    }

    #[test]
    fn cursor_allows_mid_line_edits() {
        let mut app = app();

        app.input = "helo".to_string();
        app.input_cursor = 2;
        app.handle_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE));

        assert_eq!(app.input, "hell");
        assert_eq!(app.input_cursor, 4);
    }

    #[test]
    fn shift_enter_inserts_newline() {
        let mut app = app();

        app.input = "one".to_string();
        app.input_cursor = 3;
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE));

        assert_eq!(app.input, "one\nt");
        assert_eq!(app.input_cursor, 5);
    }

    #[test]
    fn alt_enter_also_inserts_newline() {
        let mut app = app();

        app.input = "one".to_string();
        app.input_cursor = 3;
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT));

        assert_eq!(app.input, "one\n");
        assert_eq!(app.input_cursor, 4);
    }

    #[test]
    fn up_and_down_move_between_input_lines_keeping_column() {
        let mut app = app();

        app.input = "first line\nsecond\nthird line".to_string();
        // Cursor at column 8 of the last line ("third li|ne").
        app.input_cursor = "first line\nsecond\nthird li".chars().count();

        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        // "second" has 6 chars; column clamps to its end.
        assert_eq!(app.input_cursor, "first line\nsecond".chars().count());

        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.input_cursor, 6, "column carries to the longer line");

        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(
            app.input_cursor,
            "first line\nsecond\nthird ".chars().count()
        );
    }

    #[test]
    fn home_and_end_are_line_local_in_multiline_input() {
        let mut app = app();

        app.input = "first\nsecond".to_string();
        app.input_cursor = "first\nsec".chars().count();

        app.handle_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        assert_eq!(app.input_cursor, "first\n".chars().count());

        app.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        assert_eq!(app.input_cursor, "first\nsecond".chars().count());
    }

    #[test]
    fn composer_attachment_preview_has_fixed_height_and_overflow() {
        let attachments = vec![
            image_attachment("one"),
            image_attachment("two"),
            image_attachment("three"),
        ];
        let previews = attachments
            .iter()
            .map(|attachment| {
                (
                    attachment.id.clone(),
                    image_preview_lines(attachment, COMPOSER_IMAGE_PREVIEW_WIDTH),
                )
            })
            .collect::<HashMap<_, _>>();

        let lines = composer_attachment_preview_lines(&attachments, &previews, 42);
        let text = lines.iter().map(line_text).collect::<Vec<_>>();

        assert_eq!(lines.len(), COMPOSER_IMAGE_PREVIEW_HEIGHT as usize);
        assert!(text[0].contains("+1"));
        assert!(text.iter().any(|line| line.contains("320×180")));
    }

    #[test]
    fn transcript_rows_reserve_real_image_area_for_attachments() {
        let attachment = image_attachment("screenshot");
        let transcript = vec![TranscriptItem::Message(ChatMessage::user_with_attachments(
            "look at this",
            vec![attachment.clone()],
        ))];

        let rows = visible_transcript_rows(&transcript, None, None, RenderContext::static_view());
        let image_rows = rows.iter().filter(|row| row.image.is_some()).count();
        let text = rows
            .iter()
            .map(|row| line_text(&row.line))
            .collect::<Vec<_>>();

        assert_eq!(image_rows, 1);
        assert!(
            rows.iter()
                .any(|row| row.image.as_ref() == Some(&attachment))
        );
        assert!(text.iter().any(|line| line.contains("look at this")));
        assert!(text.iter().any(|line| line.contains("screenshot.png")));
        assert!(rows.len() >= CHAT_IMAGE_PREVIEW_HEIGHT as usize);
    }

    #[test]
    fn transcript_image_placement_survives_partial_scroll() {
        let attachment = image_attachment("screenshot");
        let mut rows = vec![
            TranscriptRow::text(Line::from("before image")),
            TranscriptRow::image(Line::from("image placeholder"), attachment.clone()),
        ];
        rows.extend((1..CHAT_IMAGE_PREVIEW_HEIGHT).map(|_| TranscriptRow::text(Line::from(""))));
        let area = Rect::new(0, 0, 80, 6);

        let placements = transcript_image_placements(&rows, area, 3);

        assert_eq!(placements.len(), 1);
        assert_eq!(placements[0].attachment, attachment);
        assert_eq!(placements[0].width, CHAT_IMAGE_PREVIEW_WIDTH);
        assert_eq!(placements[0].height, CHAT_IMAGE_PREVIEW_HEIGHT);
        assert_eq!(placements[0].x_offset, 2);
        assert_eq!(placements[0].y_offset, -2);
    }

    #[test]
    fn transcript_image_placement_skips_images_above_viewport() {
        let attachment = image_attachment("screenshot");
        let mut rows = vec![
            TranscriptRow::text(Line::from("before image")),
            TranscriptRow::image(Line::from("image placeholder"), attachment),
        ];
        rows.extend((1..CHAT_IMAGE_PREVIEW_HEIGHT).map(|_| TranscriptRow::text(Line::from(""))));
        let area = Rect::new(0, 0, 80, 6);

        let placements =
            transcript_image_placements(&rows, area, CHAT_IMAGE_PREVIEW_HEIGHT as usize + 2);

        assert!(placements.is_empty());
    }

    #[test]
    fn images_command_is_gone_but_ctrl_o_still_previews() {
        let mut app = app();
        app.pending_attachments.push(image_attachment("clipboard"));

        assert!(app.run_local_tool_command("/images"));
        assert_ne!(app.active_modal, Some(Modal::ImagePreview));

        app.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
        assert_eq!(app.active_modal, Some(Modal::ImagePreview));
    }

    #[test]
    fn ctrl_o_opens_latest_image_preview() {
        let mut app = app();
        app.transcript
            .push(TranscriptItem::Message(ChatMessage::user_with_attachments(
                "look",
                vec![image_attachment("one"), image_attachment("two")],
            )));

        app.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));

        assert_eq!(app.active_modal, Some(Modal::ImagePreview));
        assert_eq!(app.image_preview_index, 1);
    }

    #[test]
    fn image_preview_navigation_and_zoom_are_bounded() {
        let mut app = app();
        app.pending_attachments.push(image_attachment("one"));
        app.pending_attachments.push(image_attachment("two"));
        app.open_image_preview(0);

        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(app.image_preview_index, 1);
        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(app.image_preview_index, 0);

        for _ in 0..20 {
            app.handle_key(KeyEvent::new(KeyCode::Char('+'), KeyModifiers::NONE));
        }
        assert_eq!(app.image_preview_zoom, IMAGE_PREVIEW_MAX_ZOOM);
        for _ in 0..20 {
            app.handle_key(KeyEvent::new(KeyCode::Char('-'), KeyModifiers::NONE));
        }
        assert_eq!(app.image_preview_zoom, IMAGE_PREVIEW_MIN_ZOOM);
        app.handle_key(KeyEvent::new(KeyCode::Char('0'), KeyModifiers::NONE));
        assert_eq!(app.image_preview_zoom, 100);
    }

    #[test]
    fn ctrl_d_detaches_latest_pending_attachment() {
        let mut app = app();
        let first = image_attachment("one");
        let second = image_attachment("two");
        app.cache_attachment_preview(&first);
        app.cache_attachment_preview(&second);
        app.pending_attachments.push(first.clone());
        app.pending_attachments.push(second.clone());

        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));

        assert_eq!(app.pending_attachments, vec![first]);
        assert!(!app.attachment_previews.contains_key(&second.id));
        assert_eq!(app.status_line, "detached latest image");
    }

    #[test]
    fn preview_delete_detaches_pending_image_and_keeps_sent_images() {
        let mut app = app();
        let sent = image_attachment("sent");
        let pending = image_attachment("pending");
        app.transcript
            .push(TranscriptItem::Message(ChatMessage::user_with_attachments(
                "sent",
                vec![sent.clone()],
            )));
        app.pending_attachments.push(pending);
        app.open_latest_image_preview();

        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));

        assert!(app.pending_attachments.is_empty());
        assert_eq!(app.active_modal, Some(Modal::ImagePreview));
        assert_eq!(app.current_preview_image(), Some(sent));
        assert_eq!(app.image_preview_index, 0);
    }

    #[test]
    fn preview_delete_refuses_sent_image() {
        let mut app = app();
        let sent = image_attachment("sent");
        app.transcript
            .push(TranscriptItem::Message(ChatMessage::user_with_attachments(
                "sent",
                vec![sent.clone()],
            )));
        app.open_image_preview(0);

        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));

        assert_eq!(app.current_preview_image(), Some(sent));
        assert_eq!(app.active_modal, Some(Modal::ImagePreview));
        assert_eq!(app.status_line, "sent image stays in transcript");
    }

    #[test]
    fn image_input_warning_only_shows_for_chat_backends() {
        assert_eq!(image_input_warning("codex"), None);
        assert!(image_input_warning("deepseek").is_some());
        assert!(image_input_warning("openai-compatible").is_some());
    }

    #[test]
    fn clicking_transcript_image_opens_preview() {
        let mut app = app();
        let attachment = image_attachment("screenshot");
        app.pending_attachments.push(attachment.clone());
        app.last_chat_viewport = Some(Rect::new(0, 0, 80, 20));
        let mut rows = vec![
            TranscriptRow::text(Line::from("before image")),
            TranscriptRow::image(Line::from("image placeholder"), attachment),
        ];
        rows.extend((1..CHAT_IMAGE_PREVIEW_HEIGHT).map(|_| TranscriptRow::text(Line::from(""))));
        app.last_transcript_rows = Arc::new(rows);

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 4,
            row: 2,
            modifiers: KeyModifiers::NONE,
        });

        assert_eq!(app.active_modal, Some(Modal::ImagePreview));
        assert_eq!(app.image_preview_index, 0);
    }

    #[test]
    fn preview_image_dimensions_fit_at_default_zoom_and_scale_up() {
        let attachment = image_attachment("wide");
        let area = Rect::new(0, 0, 80, 20);

        let fit = preview_image_dimensions(&attachment, area, 100);
        let zoomed = preview_image_dimensions(&attachment, area, 200);

        assert!(fit.0 <= area.width);
        assert!(fit.1 <= area.height);
        assert!(zoomed.0 >= fit.0);
        assert!(zoomed.1 >= fit.1);
    }

    #[test]
    fn enter_captures_task() {
        let mut app = app();

        app.input = "fix tests".to_string();
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(app.input, "");
        assert_eq!(
            app.transcript,
            vec![TranscriptItem::Message(ChatMessage::user("fix tests"))]
        );
    }

    #[test]
    fn control_j_submits_task_for_pty_enter() {
        let mut app = app();

        app.input = "fix tests".to_string();
        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));

        assert_eq!(app.input, "");
        assert_eq!(
            app.transcript,
            vec![TranscriptItem::Message(ChatMessage::user("fix tests"))]
        );
    }

    #[test]
    fn help_command_lists_slash_commands() {
        let mut app = app();

        app.input = "/help".to_string();
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(app.status_line, "help opened");
        assert_eq!(app.active_modal, Some(Modal::Help));
    }

    #[test]
    fn settings_command_opens_settings_modal() {
        let mut app = app();
        let expected_theme = app.theme.name().to_string();

        app.input = "/settings".to_string();
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(app.status_line, "settings opened");
        assert_eq!(app.active_modal, Some(Modal::Settings));
        assert!(
            app.settings_rows()
                .iter()
                .any(|(key, value)| { *key == "model" && value == "gpt-5.5" })
        );
        assert!(
            app.settings_rows()
                .iter()
                .any(|(key, value)| { *key == "theme" && value == &expected_theme })
        );
        assert!(
            app.settings_rows()
                .iter()
                .any(|(key, value)| { *key == "permissions" && value == "open" })
        );
    }

    #[test]
    fn model_command_switches_model_and_persists_setting() {
        let (mut app, workspace) = app_in_workspace();

        app.input = "/model gpt-test-model".to_string();
        app.input_cursor = app.input_len();
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(app.model.model_name(), "gpt-test-model");
        assert_eq!(app.status_line, "model: gpt-test-model");
        let settings = load_app_settings(&workspace).unwrap();
        assert_eq!(settings.model(), Some("gpt-test-model".to_string()));
    }

    #[test]
    fn selecting_model_from_palette_opens_model_picker() {
        let mut app = app();

        app.input = "/model".to_string();
        app.input_cursor = app.input_len();
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(app.active_modal, Some(Modal::Models));
        assert_eq!(app.input, "");
        assert_eq!(app.status_line, "models opened");
    }

    #[test]
    fn reasoning_command_sets_effort_and_persists_setting() {
        let (mut app, workspace) = app_in_workspace();

        app.input = "/reasoning high".to_string();
        app.input_cursor = app.input_len();
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(app.model.reasoning_effort(), "high");
        assert_eq!(app.status_line, "reasoning: high");
        let settings = load_app_settings(&workspace).unwrap();
        assert_eq!(settings.reasoning_effort(), Some("high".to_string()));
    }

    #[test]
    fn reasoning_command_opens_picker_and_enter_saves_selection() {
        let (mut app, _workspace) = app_in_workspace();

        app.input = "/reasoning".to_string();
        app.input_cursor = app.input_len();
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.active_modal, Some(Modal::Reasoning));

        // The picker is seeded to the active effort; moving + Enter applies a
        // different one and closes.
        let before = app.model.reasoning_effort().to_string();
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.active_modal, None);
        assert_ne!(app.model.reasoning_effort(), before);
    }

    #[test]
    fn reasoning_effort_preference_round_trips_through_settings() {
        let workspace = temp_workspace();
        save_reasoning_preference(&workspace, "xhigh").unwrap();
        let settings = load_app_settings(&workspace).unwrap();
        assert_eq!(settings.reasoning_effort(), Some("xhigh".to_string()));
        // Blank/whitespace is normalized away so the backend default applies.
        save_reasoning_preference(&workspace, "   ").unwrap();
        assert_eq!(
            load_app_settings(&workspace).unwrap().reasoning_effort(),
            None
        );
    }

    #[test]
    fn permission_command_switches_mode_and_updates_runtime() {
        let (mut app, workspace) = app_in_workspace();

        app.input = "/permissions readonly".to_string();
        app.input_cursor = app.input_len();
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(app.permission_mode, PermissionMode::Readonly);
        assert_eq!(app.status_line, "permissions: readonly");
        let settings = load_app_settings(&workspace).unwrap();
        assert_eq!(settings.permission_mode(), PermissionMode::Readonly);
        let denied = app
            .tools
            .file_patch(FilePatchRequest::new(
                "*** Begin Patch\n*** Add File: writable.txt\n+nope\n*** End Patch\n",
            ))
            .unwrap_err()
            .to_string();
        assert!(denied.contains("does not match an allow_prefixes entry"));
    }

    #[test]
    fn readonly_mode_is_visible_in_header_and_status() {
        let mut app = app();
        app.permission_mode = PermissionMode::Readonly;

        let (label, _) = app.header_state();

        assert_eq!(label, "readonly");
        assert_eq!(app.scoped_status("streaming"), "readonly · streaming");
    }

    #[test]
    fn conversation_history_includes_permission_context() {
        let mut app = app();
        app.permission_mode = PermissionMode::Readonly;
        app.transcript
            .push(TranscriptItem::Message(ChatMessage::user("read codebase")));

        let messages = app.conversation_history();

        assert_eq!(
            messages.first().map(|message| message.role.as_str()),
            Some("system")
        );
        assert!(
            messages
                .first()
                .is_some_and(|message| message.content.contains("permission mode: readonly"))
        );
        assert!(messages.first().is_some_and(|message| {
            message
                .content
                .contains("File mutation tools are unavailable")
        }));
        assert!(
            messages
                .iter()
                .any(|message| message.content == "read codebase")
        );
    }

    #[test]
    fn conversation_history_includes_rolling_session_state() {
        let mut app = app();
        for index in 0..40 {
            app.transcript
                .push(TranscriptItem::Message(ChatMessage::user(format!(
                    "old task {index}"
                ))));
            app.transcript
                .push(TranscriptItem::Message(ChatMessage::assistant(format!(
                    "old outcome {index}"
                ))));
        }

        let messages = app.conversation_history();

        assert_eq!(
            messages.get(1).map(|message| message.role.as_str()),
            Some("system")
        );
        assert!(messages[1].content.contains("Medusa rolling session state"));
        assert!(messages[1].content.contains("old task 39"));
        // Full history flows through; the ContextEngine compacts at turn
        // start only when the token budget requires it.
        assert!(
            messages
                .iter()
                .any(|message| message.role == "user" && message.content == "old task 0")
        );
        assert!(
            messages
                .iter()
                .any(|message| message.role == "user" && message.content == "old task 39")
        );
    }

    #[test]
    fn session_state_preserves_semantic_memory_outside_recent_window() {
        let mut app = app();
        app.transcript
            .push(TranscriptItem::Message(ChatMessage::user(
                "I prefer concise answers and do not touch auth code.",
            )));
        for index in 0..40 {
            app.transcript
                .push(TranscriptItem::Message(ChatMessage::user(format!(
                    "noise task {index}"
                ))));
            app.transcript
                .push(TranscriptItem::Message(ChatMessage::assistant(format!(
                    "noise outcome {index}"
                ))));
        }

        let messages = app.conversation_history();

        assert!(messages[1].content.contains("semantic memory"));
        assert!(
            messages[1]
                .content
                .contains("preference: I prefer concise answers")
        );
    }

    #[test]
    fn session_state_summarizes_tool_file_mentions() {
        let mut app = app();
        app.transcript.push(TranscriptItem::Tool(ToolRun {
            id: None,
            started_at: Instant::now(),
            pending_result: None,
            name: "file.patch".to_string(),
            summary: "edited crates/medusa-tui/src/main.rs".to_string(),
            state: ToolRunState::Succeeded,
            detail: "also touched README.md".to_string(),
            expanded: false,
            group_expanded: false,
        }));

        let messages = app.conversation_history();

        assert!(messages[1].content.contains("tool history"));
        assert!(messages[1].content.contains("file.patch succeeded"));
        assert!(messages[1].content.contains("changed or referenced files"));
        assert!(
            messages[1]
                .content
                .contains("crates/medusa-tui/src/main.rs")
        );
        assert!(messages[1].content.contains("README.md"));
    }

    #[test]
    fn settings_modal_can_open_model_and_permission_pickers() {
        let mut app = app();

        app.open_settings_modal();
        app.settings_selection = app
            .settings_items()
            .iter()
            .position(|item| item.key == "model")
            .unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.active_modal, Some(Modal::Models));

        app.open_settings_modal();
        app.settings_selection = app
            .settings_items()
            .iter()
            .position(|item| item.key == "permissions")
            .unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.active_modal, Some(Modal::Permissions));
    }

    #[test]
    fn slash_prefix_suggests_settings() {
        let mut app = app();

        app.input = "/se".to_string();
        app.input_cursor = 3;

        let matches = app.slash_matches();
        assert!(
            matches
                .iter()
                .any(|(command, _)| command.name == "/settings")
        );
    }

    #[test]
    fn fuzzy_subsequence_matches_commands() {
        let mut app = app();

        app.input = "/wf".to_string();
        app.input_cursor = 3;

        let matches = app.slash_matches();
        let workflow = matches
            .iter()
            .find(|(command, _)| command.name == "/workflow")
            .expect("fuzzy match for /workflow");
        assert_eq!(workflow.1, vec![0, 4]);
    }

    #[test]
    fn enter_on_fully_typed_command_runs_it_directly() {
        let mut app = app();

        app.input = "/help".to_string();
        app.input_cursor = app.input_len();
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(app.active_modal, Some(Modal::Help));
        assert!(app.input.is_empty());
    }

    #[test]
    fn slash_prefix_suggests_fork() {
        let mut app = app();

        app.input = "/fo".to_string();
        app.input_cursor = 3;

        let matches = app.slash_matches();
        assert!(matches.iter().any(|(command, _)| command.name == "/fork"));
    }

    #[test]
    fn slash_prefix_suggests_rewind() {
        let mut app = app();

        app.input = "/re".to_string();
        app.input_cursor = 3;

        let matches = app.slash_matches();
        assert!(matches.iter().any(|(command, _)| command.name == "/rewind"));
    }

    fn checkpoint_in(workspace: &Path, prompt: &str, user_index: usize) -> String {
        let recorder = CheckpointRecorder::new(
            workspace,
            CheckpointMeta {
                session_id: "session-elsewhere.json".to_string(),
                prompt_excerpt: prompt.to_string(),
                transcript_user_index: user_index,
            },
        );
        fs::write(workspace.join("tracked.txt"), format!("{prompt}\n")).unwrap();
        recorder.capture(&["tracked.txt".to_string()]).unwrap();
        fs::write(workspace.join("tracked.txt"), "mutated\n").unwrap();
        recorder.finish().unwrap().id
    }

    #[test]
    fn rewind_opens_modal_with_entries_newest_first() {
        let mut app = app();
        let workspace = app.tools.workspace().to_path_buf();
        let first = checkpoint_in(&workspace, "first turn", 0);
        let second = checkpoint_in(&workspace, "second turn", 2);

        assert!(app.run_local_tool_command("/rewind"));

        assert_eq!(app.active_modal, Some(Modal::Rewind));
        assert_eq!(app.rewind_stage, RewindStage::Pick);
        assert_eq!(app.rewind_entries.len(), 2);
        assert_eq!(app.rewind_entries[0].id, second);
        assert_eq!(app.rewind_entries[1].id, first);
        assert_eq!(app.rewind_entries[0].prompt_excerpt, "second turn");
    }

    #[test]
    fn rewind_without_checkpoints_stays_closed() {
        let mut app = app();

        assert!(app.run_local_tool_command("/rewind"));

        assert_eq!(app.active_modal, None);
        assert_eq!(app.status_line, "no checkpoints yet");
    }

    #[test]
    fn rewind_is_refused_while_a_turn_is_streaming() {
        let mut app = app();
        let workspace = app.tools.workspace().to_path_buf();
        checkpoint_in(&workspace, "some turn", 0);
        let (_sender, receiver) = mpsc::channel::<ModelStreamEvent>();
        app.model_events = Some(receiver);

        assert!(app.run_local_tool_command("/rewind"));

        assert_eq!(app.active_modal, None);
        assert_eq!(app.status_line, "finish the current turn before rewinding");
    }

    #[test]
    fn rewind_restore_rewinds_files_and_closes_modal() {
        let mut app = app();
        let workspace = app.tools.workspace().to_path_buf();
        let id = checkpoint_in(&workspace, "edit tracked", 0);
        assert_eq!(
            fs::read_to_string(workspace.join("tracked.txt")).unwrap(),
            "mutated\n"
        );

        assert!(app.run_local_tool_command("/rewind"));
        assert_eq!(app.rewind_entries[0].id, id);
        // Pick the checkpoint, then confirm the default "Restore files".
        app.handle_rewind_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.rewind_stage, RewindStage::Confirm);
        // Foreign-session checkpoint: no fork option offered.
        assert_eq!(
            app.rewind_confirm_options(),
            vec!["Restore files", "Cancel"]
        );
        app.handle_rewind_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(app.active_modal, None);
        assert_eq!(
            fs::read_to_string(workspace.join("tracked.txt")).unwrap(),
            "edit tracked\n"
        );
    }

    #[test]
    fn rewind_fork_truncates_transcript_and_prefills_composer() {
        let mut app = app();
        let workspace = app.tools.workspace().to_path_buf();
        let session = SessionStore::open(&workspace, SessionOpenMode::New).unwrap();
        let original_id = session.current_id();
        session.save_transcript(&[]).unwrap();
        app.session = Some(session);
        app.transcript = vec![
            TranscriptItem::Message(ChatMessage::user("first prompt")),
            TranscriptItem::Message(ChatMessage::assistant("first answer")),
            TranscriptItem::Message(ChatMessage::user("second prompt")),
            TranscriptItem::Message(ChatMessage::assistant("second answer")),
        ];
        let entry = CheckpointEntry {
            id: "cp-test".to_string(),
            session_id: original_id.clone(),
            created_at_ms: 0,
            prompt_excerpt: "second prompt".to_string(),
            transcript_user_index: 2,
            parent_id: None,
            note: String::new(),
            files: Vec::new(),
        };

        app.fork_transcript_at_checkpoint(&entry);

        assert_eq!(app.transcript.len(), 2);
        assert!(matches!(
            &app.transcript[1],
            TranscriptItem::Message(message) if message.content == "first answer"
        ));
        assert_eq!(app.input, "second prompt");
        let forked_id = app.session.as_ref().unwrap().current_id();
        assert_ne!(forked_id, original_id);
        assert_eq!(
            app.session.as_ref().unwrap().parent_id(),
            Some(original_id.as_str())
        );
    }

    /// Finding [20]: `/clear` empties the transcript without rotating the
    /// session id, so a pre-clear checkpoint's `transcript_user_index` is now
    /// stale. Session-id equality alone must NOT offer fork.
    #[test]
    fn rewind_hides_fork_for_stale_index_after_clear() {
        let mut app = app();
        let workspace = app.tools.workspace().to_path_buf();
        let session = SessionStore::open(&workspace, SessionOpenMode::New).unwrap();
        let session_id = session.current_id();
        app.session = Some(session);
        // Same session id, index 8 recorded before /clear.
        app.rewind_entries = vec![CheckpointEntry {
            id: "cp-stale".to_string(),
            session_id: session_id.clone(),
            created_at_ms: 1,
            prompt_excerpt: "old prompt".to_string(),
            transcript_user_index: 8,
            parent_id: None,
            note: String::new(),
            files: Vec::new(),
        }];
        app.rewind_selection = 0;
        // /clear then one fresh turn: the transcript no longer has row 8.
        app.transcript = vec![
            TranscriptItem::Message(ChatMessage::user("brand new prompt")),
            TranscriptItem::Message(ChatMessage::assistant("answer")),
        ];

        assert!(
            !app.selected_rewind_offers_fork(),
            "stale index must not offer fork even though session id still matches"
        );
        assert_eq!(
            app.rewind_confirm_options(),
            vec!["Restore files", "Cancel"]
        );
    }

    /// Finding [20]: even if the confirm option is reached directly, forking on
    /// a stale index must not truncate the live transcript, overwrite the
    /// composer, rotate the session, or toast a false success.
    #[test]
    fn fork_at_checkpoint_refuses_stale_index_and_leaves_conversation_intact() {
        let mut app = app();
        let workspace = app.tools.workspace().to_path_buf();
        let session = SessionStore::open(&workspace, SessionOpenMode::New).unwrap();
        let original_id = session.current_id();
        session.save_transcript(&[]).unwrap();
        app.session = Some(session);
        app.transcript = vec![
            TranscriptItem::Message(ChatMessage::user("brand new prompt")),
            TranscriptItem::Message(ChatMessage::assistant("answer")),
        ];
        app.input = "unsent draft".to_string();
        let entry = CheckpointEntry {
            id: "cp-stale".to_string(),
            session_id: original_id.clone(),
            created_at_ms: 1,
            prompt_excerpt: "old prompt".to_string(),
            transcript_user_index: 8,
            parent_id: None,
            note: String::new(),
            files: Vec::new(),
        };

        app.fork_transcript_at_checkpoint(&entry);

        // Transcript untouched, draft preserved, session NOT forked.
        assert_eq!(app.transcript.len(), 2);
        assert_eq!(app.input, "unsent draft");
        assert_eq!(app.session.as_ref().unwrap().current_id(), original_id);
    }

    /// Finding [20] guardrail: a genuinely-live checkpoint (index maps to its
    /// user message, excerpt still matches) still offers and applies fork.
    #[test]
    fn rewind_still_offers_fork_for_live_checkpoint() {
        let mut app = app();
        let workspace = app.tools.workspace().to_path_buf();
        let session = SessionStore::open(&workspace, SessionOpenMode::New).unwrap();
        let session_id = session.current_id();
        app.session = Some(session);
        app.transcript = vec![
            TranscriptItem::Message(ChatMessage::user("first prompt")),
            TranscriptItem::Message(ChatMessage::assistant("first answer")),
            TranscriptItem::Message(ChatMessage::user("second prompt")),
            TranscriptItem::Message(ChatMessage::assistant("second answer")),
        ];
        app.rewind_entries = vec![CheckpointEntry {
            id: "cp-live".to_string(),
            session_id,
            created_at_ms: 1,
            prompt_excerpt: "second prompt".to_string(),
            transcript_user_index: 2,
            parent_id: None,
            note: String::new(),
            files: Vec::new(),
        }];
        app.rewind_selection = 0;

        assert!(app.selected_rewind_offers_fork());
        assert!(
            app.rewind_confirm_options()
                .contains(&"Restore files + fork conversation")
        );
    }

    #[test]
    fn edit_command_opens_picker_newest_first() {
        let mut app = app();
        app.transcript = vec![
            TranscriptItem::Message(ChatMessage::user("first prompt")),
            TranscriptItem::Message(ChatMessage::assistant("first answer")),
            TranscriptItem::Message(ChatMessage::user("second\nprompt with lines")),
            TranscriptItem::Message(ChatMessage::assistant("second answer")),
        ];

        assert!(app.run_local_tool_command("/edit"));

        assert_eq!(app.active_modal, Some(Modal::EditMessage));
        assert_eq!(app.edit_picker_selection, 0);
        assert_eq!(app.edit_picker_entries.len(), 2);
        assert_eq!(app.edit_picker_entries[0].transcript_index, 2);
        assert_eq!(
            app.edit_picker_entries[0].preview,
            "second prompt with lines"
        );
        assert_eq!(app.edit_picker_entries[1].transcript_index, 0);
        assert_eq!(app.edit_picker_entries[1].preview, "first prompt");
    }

    #[test]
    fn edit_picker_caps_at_twenty_messages() {
        let mut app = app();
        app.transcript = (0..25)
            .map(|index| TranscriptItem::Message(ChatMessage::user(format!("prompt {index}"))))
            .collect();

        assert!(app.run_local_tool_command("/edit"));

        assert_eq!(app.edit_picker_entries.len(), EDIT_PICKER_LIMIT);
        assert_eq!(app.edit_picker_entries[0].preview, "prompt 24");
    }

    #[test]
    fn edit_without_user_messages_stays_closed() {
        let mut app = app();
        app.transcript = vec![TranscriptItem::Message(ChatMessage::assistant("hi"))];

        assert!(app.run_local_tool_command("/edit"));

        assert_eq!(app.active_modal, None);
        assert_eq!(app.status_line, "no previous messages to edit");
    }

    #[test]
    fn edit_is_refused_while_a_turn_is_streaming() {
        let mut app = app();
        app.transcript = vec![TranscriptItem::Message(ChatMessage::user("prompt"))];
        let (_sender, receiver) = mpsc::channel::<ModelStreamEvent>();
        app.model_events = Some(receiver);

        assert!(app.run_local_tool_command("/edit"));

        assert_eq!(app.active_modal, None);
        assert_eq!(
            app.status_line,
            "finish the current turn before editing a message"
        );
    }

    #[test]
    fn edit_selection_forks_session_truncates_transcript_and_prefills_composer() {
        let mut app = app();
        let workspace = app.tools.workspace().to_path_buf();
        let session = SessionStore::open(&workspace, SessionOpenMode::New).unwrap();
        let original_id = session.current_id();
        let transcript = vec![
            TranscriptItem::Message(ChatMessage::user("first prompt")),
            TranscriptItem::Message(ChatMessage::assistant("first answer")),
            TranscriptItem::Message(ChatMessage::user("second prompt")),
            TranscriptItem::Message(ChatMessage::assistant("second answer")),
        ];
        session.save_transcript(&transcript).unwrap();
        app.session = Some(session);
        app.transcript = transcript;

        assert!(app.run_local_tool_command("/edit"));
        assert_eq!(app.active_modal, Some(Modal::EditMessage));
        // Newest first: selection 0 is "second prompt" at transcript index 2.
        app.handle_edit_message_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(app.active_modal, None);
        assert_eq!(app.transcript.len(), 2);
        assert!(matches!(
            &app.transcript[1],
            TranscriptItem::Message(message) if message.content == "first answer"
        ));
        assert_eq!(app.input, "second prompt");
        assert_eq!(app.input_cursor, app.input_len());
        assert!(app.status_line.starts_with("editing message"));

        // The session tree gained a fork: the live session is a new child
        // of the original, and the original file still holds the full
        // four-item timeline.
        let session = app.session.as_ref().unwrap();
        let forked_id = session.current_id();
        assert_ne!(forked_id, original_id);
        assert_eq!(session.parent_id(), Some(original_id.as_str()));
        let original_path = workspace
            .join(".medusa")
            .join("sessions")
            .join(&original_id);
        let original: medusa_core::session::LoadedSessionFile<TranscriptItem> =
            read_session_file(&original_path).unwrap();
        assert_eq!(original.transcript.len(), 4);
    }

    #[test]
    fn edit_selection_is_refused_when_a_turn_starts_while_picker_is_open() {
        let mut app = app();
        let workspace = app.tools.workspace().to_path_buf();
        let session = SessionStore::open(&workspace, SessionOpenMode::New).unwrap();
        app.session = Some(session);
        app.transcript = vec![TranscriptItem::Message(ChatMessage::user("prompt"))];

        assert!(app.run_local_tool_command("/edit"));
        let (_sender, receiver) = mpsc::channel::<ModelStreamEvent>();
        app.model_events = Some(receiver);
        app.handle_edit_message_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(app.active_modal, None);
        assert_eq!(app.transcript.len(), 1);
        assert!(app.input.is_empty());
        assert_eq!(
            app.status_line,
            "finish the current turn before editing a message"
        );
    }

    #[test]
    fn review_seeds_composer_without_sending() {
        let mut app = app();
        app.review_diff_check = |_| true;

        assert!(app.run_local_tool_command("/review"));

        assert_eq!(app.input, REVIEW_PROMPT_TEMPLATE);
        assert_eq!(app.input_cursor, app.input_len());
        assert!(app.input.contains("git status"));
        assert!(app.input.contains("git diff"));
        assert!(app.input.contains("correctness bugs first"));
        assert!(app.input.contains("file:line"));
        // Seeded only — nothing was sent and no turn started.
        assert!(app.transcript.is_empty());
        assert!(app.model_events.is_none());
        assert_eq!(
            app.status_line,
            "review prompt ready — edit and press enter"
        );
    }

    #[test]
    fn review_toasts_when_nothing_to_review() {
        let mut app = app();
        app.review_diff_check = |_| false;

        assert!(app.run_local_tool_command("/review"));

        assert!(app.input.is_empty());
        assert_eq!(app.status_line, "nothing to review");
        assert_eq!(app.toast.as_ref().unwrap().message, "Nothing to review");
    }

    #[test]
    fn reviewable_diff_probe_reports_repo_and_diff_state() {
        // Not a git repo: nothing to review.
        let bare = temp_workspace();
        assert!(!workspace_has_reviewable_diff(&bare));

        // Fresh repo with no changes at all: still nothing to review.
        let repo = temp_workspace();
        let init = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repo)
            .status()
            .unwrap();
        assert!(init.success());
        assert!(!workspace_has_reviewable_diff(&repo));

        // A pending (untracked) file makes the workspace reviewable.
        fs::write(repo.join("pending.txt"), "change\n").unwrap();
        assert!(workspace_has_reviewable_diff(&repo));
    }

    #[test]
    fn slash_prefix_suggests_resume() {
        let mut app = app();

        app.input = "/re".to_string();
        app.input_cursor = 3;

        let matches = app.slash_matches();
        assert!(matches.iter().any(|(command, _)| command.name == "/resume"));
    }

    #[test]
    fn slash_prefix_suggests_tree() {
        let mut app = app();

        app.input = "/tr".to_string();
        app.input_cursor = 3;

        let matches = app.slash_matches();
        assert!(matches.iter().any(|(command, _)| command.name == "/tree"));
    }

    #[test]
    fn slash_prefix_suggests_skills() {
        let mut app = app();

        app.input = "/sk".to_string();
        app.input_cursor = 3;

        let matches = app.slash_matches();
        assert!(matches.iter().any(|(command, _)| command.name == "/skills"));
    }

    #[test]
    fn slash_prefix_suggests_workflow_commands() {
        let mut app = app();

        app.input = "/work".to_string();
        app.input_cursor = app.input_len();

        let matches = app.slash_matches();
        assert!(
            matches
                .iter()
                .any(|(command, _)| command.name == "/workflow")
        );
        assert!(
            matches
                .iter()
                .any(|(command, _)| command.name == "/workflows")
        );
    }

    #[test]
    fn slash_search_matches_description_and_category() {
        let mut app = app();

        app.input = "/switch".to_string();
        app.input_cursor = app.input_len();

        let matches = app.slash_matches();
        assert!(matches.iter().any(|(command, _)| command.name == "/theme"));

        app.input = "/session".to_string();
        app.input_cursor = app.input_len();
        let matches = app.slash_matches();
        assert!(matches.iter().any(|(command, _)| command.name == "/resume"));
    }

    #[test]
    fn command_palette_is_centered() {
        let rect = command_palette_rect(Rect::new(0, 0, 100, 40), 10);

        assert_eq!(rect.width, 92);
        assert_eq!(rect.height, 15);
        assert_eq!(rect.x, 4);
        assert_eq!(rect.y, 12);
    }

    #[test]
    fn ctrl_p_opens_command_palette() {
        let mut app = app();

        app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));

        assert_eq!(app.input, "/");
        assert_eq!(app.input_cursor, 1);
        assert!(app.slash_suggestions_active());
    }

    #[test]
    fn command_palette_navigation_uses_dedicated_keys() {
        let mut app = app();

        app.open_command_palette();
        app.handle_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
        assert!(app.slash_selection > 0);

        app.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        assert_eq!(app.slash_selection, app.slash_matches().len() - 1);

        app.handle_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        assert_eq!(app.slash_selection, 0);

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.input, "");
        assert!(!app.should_quit);
    }

    #[test]
    fn command_palette_tab_navigation_cycles_suggestions() {
        let mut app = app();

        app.open_command_palette();
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.slash_selection, 1);

        app.handle_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
        assert_eq!(app.slash_selection, 0);
    }

    #[test]
    fn enter_accepts_slash_suggestion() {
        let mut app = app();

        app.input = "/se".to_string();
        app.input_cursor = 3;
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(app.active_modal, Some(Modal::Settings));
        assert_eq!(app.input, "");
    }

    #[test]
    fn tools_command_lists_minimal_surface() {
        let mut app = app();

        app.input = "/tools".to_string();
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(matches!(
            &app.transcript[..],
            [TranscriptItem::Message(ChatMessage { role: ChatRole::System, content, .. })]
                if content.contains("terminal.exec") && content.contains("file.patch")
        ));
    }

    #[test]
    fn reload_command_requests_restart() {
        let mut app = app();

        assert!(app.run_local_tool_command("/reload"));

        assert!(app.restart_requested);
        assert!(app.should_quit);
        assert_eq!(app.status_line, "reloading Medusa…");
    }

    #[test]
    fn reload_command_refuses_active_work() {
        let mut app = app();
        let (_sender, receiver) = mpsc::channel();
        app.model_events = Some(receiver);

        assert!(app.run_local_tool_command("/reload"));

        assert!(!app.restart_requested);
        assert!(!app.should_quit);
        assert_eq!(app.status_line, "reload blocked: work is still running");
    }

    #[test]
    fn reload_command_is_listed() {
        assert!(
            SLASH_COMMANDS
                .iter()
                .any(|command| command.name == "/reload")
        );
    }

    #[test]
    fn reload_rebuild_probe_ignores_non_workspace_binary() {
        maybe_rebuild_before_reload(Path::new("/tmp/medusa-not-from-this-workspace"))
            .expect("non-workspace binaries should reload without a rebuild probe");
    }

    #[test]
    fn themes_command_opens_theme_modal() {
        let mut app = app();

        app.input = "/theme".to_string();
        app.input_cursor = app.input_len();
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(app.status_line, "themes opened");
        assert_eq!(app.active_modal, Some(Modal::Themes));
        assert_eq!(app.theme_selection, theme_index(app.theme));
    }

    #[test]
    fn selecting_theme_from_palette_opens_theme_menu_without_input_arg() {
        let mut app = app();

        app.input = "/theme".to_string();
        app.input_cursor = app.input_len();
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(app.active_modal, Some(Modal::Themes));
        assert_eq!(app.input, "");
        assert_eq!(app.status_line, "themes opened");
    }

    #[test]
    fn theme_modal_can_apply_selection_with_keyboard() {
        let mut app = app();
        app.set_theme(ThemeKind::Medusa);

        app.open_themes_modal();
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(app.theme, ThemeKind::OpenCode);
        assert_eq!(app.active_modal, None);
        assert_eq!(app.status_line, "theme: opencode");
    }

    #[test]
    fn theme_modal_previews_theme_while_navigating() {
        let mut app = app();
        app.theme = ThemeKind::Medusa;
        app.theme_selection = theme_index(app.theme);
        set_active_theme(app.theme);

        app.open_themes_modal();
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));

        assert_eq!(app.theme, ThemeKind::OpenCode);
        assert_eq!(app.theme_preview_original, Some(ThemeKind::Medusa));
        assert_eq!(app.status_line, "preview theme: opencode");
    }

    #[test]
    fn theme_preview_restyles_cached_chat_and_tool_rows() {
        let mut app = app();
        app.theme = ThemeKind::Medusa;
        app.theme_selection = theme_index(app.theme);
        set_active_theme(app.theme);
        app.transcript = vec![
            TranscriptItem::Message(ChatMessage::user("hello")),
            TranscriptItem::Tool(ToolRun {
                id: None,
                started_at: Instant::now(),
                pending_result: None,
                name: "terminal.exec".to_string(),
                summary: "$ cargo test".to_string(),
                state: ToolRunState::Succeeded,
                detail: "done".to_string(),
                expanded: false,
                group_expanded: false,
            }),
        ];
        app.attachment_previews.insert(
            "cached".to_string(),
            vec![Line::from(Span::styled("old", accent()))],
        );
        app.touch_transcript();

        app.visible_transcript_rows_cached();
        assert!(app.transcript_rows_cache.is_some());
        assert_eq!(
            app.transcript_rows_cache.as_ref().map(|cache| cache.theme),
            Some(ThemeKind::Medusa)
        );

        app.open_themes_modal();
        app.theme_selection = theme_index(ThemeKind::MaterialAmber);
        app.preview_theme_selection();

        assert!(app.transcript_rows_cache.is_none());
        assert!(app.last_transcript_rows.is_empty());
        assert!(app.attachment_previews.is_empty());

        let updated = app.visible_transcript_rows_cached();
        assert_eq!(
            app.transcript_rows_cache.as_ref().map(|cache| cache.theme),
            Some(ThemeKind::MaterialAmber)
        );
        assert!(first_span_fg_containing(&updated, "›").is_some());
        assert!(first_span_fg_containing(&updated, "terminal").is_some());
    }

    #[test]
    fn theme_modal_escape_restores_previewed_theme() {
        let mut app = app();
        app.theme = ThemeKind::Medusa;
        app.theme_selection = theme_index(app.theme);
        set_active_theme(app.theme);

        app.open_themes_modal();
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        assert_eq!(app.theme, ThemeKind::Medusa);
        assert_eq!(app.theme_preview_original, None);
        assert_eq!(app.active_modal, None);
        assert_eq!(app.status_line, "closed");
    }

    #[test]
    fn settings_modal_can_open_theme_editor_from_menu() {
        let mut app = app();

        app.open_settings_modal();
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(app.active_modal, Some(Modal::Themes));
        assert_eq!(app.theme_selection, theme_index(app.theme));
    }

    #[test]
    fn theme_command_switches_active_theme() {
        let mut app = app();

        app.input = "/theme opencode".to_string();
        app.input_cursor = app.input_len();
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(app.theme, ThemeKind::OpenCode);
        assert_eq!(app.status_line, "theme: opencode");
    }

    #[test]
    fn theme_command_cycles_next_and_previous() {
        let (mut app, _workspace) = app_in_workspace();
        app.theme = ThemeKind::Medusa;
        app.theme_selection = theme_index(app.theme);
        set_active_theme(app.theme);

        assert!(app.run_local_tool_command("/theme next"));
        assert_eq!(app.theme, ThemeKind::OpenCode);
        assert_eq!(app.theme_selection, theme_index(ThemeKind::OpenCode));

        assert!(app.run_local_tool_command("/theme prev"));
        assert_eq!(app.theme, ThemeKind::Medusa);

        assert!(app.run_local_tool_command("/theme previous"));
        assert_eq!(app.theme, ThemeKind::Vesper);
        assert_eq!(app.status_line, "theme: vesper");
    }

    #[test]
    fn slash_theme_prefix_suggests_theme_names() {
        let mut app = app();
        app.input = "/theme mat".to_string();
        app.input_cursor = app.input_len();

        let names = app
            .slash_matches()
            .into_iter()
            .map(|(command, _)| command.name)
            .collect::<Vec<_>>();

        assert!(names.contains(&"/theme material-dark"));
        assert!(names.contains(&"/theme material-amber"));
        assert!(!names.contains(&"/help"));
    }

    #[test]
    fn theme_preview_lines_include_labeled_swatches() {
        let preview = theme_preview_lines(ThemeKind::MaterialTeal)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>();

        for label in ["accent", "prompt", "tool", "success", "error"] {
            assert!(
                preview.iter().any(|line| line.contains(label)),
                "missing {label} swatch"
            );
        }
        assert!(
            preview
                .iter()
                .any(|line| line.contains("selection / focus"))
        );
        assert!(preview.iter().any(|line| line.contains("inline code")));
    }

    #[test]
    fn additional_themes_are_listed_and_selectable() {
        let expected = [
            ("dracula", ThemeKind::Dracula),
            ("nord", ThemeKind::Nord),
            ("gruvbox", ThemeKind::Gruvbox),
            ("solarized-dark", ThemeKind::SolarizedDark),
            ("material-dark", ThemeKind::MaterialDark),
            ("material-teal", ThemeKind::MaterialTeal),
            ("material-amber", ThemeKind::MaterialAmber),
            ("material-indigo", ThemeKind::MaterialIndigo),
            ("material-rose", ThemeKind::MaterialRose),
        ];

        for (name, kind) in expected {
            assert!(ThemeKind::all().contains(&kind));
            assert_eq!(ThemeKind::from_name(name), Some(kind));
            assert_eq!(kind.name(), name);
        }

        assert_eq!(
            ThemeKind::from_name("material"),
            Some(ThemeKind::MaterialDark)
        );
        assert_eq!(
            ThemeKind::from_name("material-cyan"),
            Some(ThemeKind::MaterialTeal)
        );
        assert_eq!(
            ThemeKind::from_name("material-purple"),
            Some(ThemeKind::MaterialIndigo)
        );
        assert_eq!(
            ThemeKind::from_name("material-pink"),
            Some(ThemeKind::MaterialRose)
        );
    }

    #[test]
    fn material_themes_use_distinct_accents() {
        let themes = [
            ThemeKind::MaterialDark,
            ThemeKind::MaterialTeal,
            ThemeKind::MaterialAmber,
            ThemeKind::MaterialIndigo,
            ThemeKind::MaterialRose,
        ];

        for theme in themes {
            let palette = theme.palette();
            assert_eq!(palette.success, MATERIAL_GREEN_400);
            assert_eq!(palette.error, MATERIAL_RED_400);
            assert_eq!(palette.separator, MATERIAL_BLUE_GREY_800);
        }

        assert_ne!(
            ThemeKind::MaterialTeal.palette().accent,
            ThemeKind::MaterialAmber.palette().accent
        );
    }

    #[test]
    fn skills_command_lists_workspace_skills() {
        let workspace = temp_workspace();
        let skill_dir = workspace.join(".medusa/skills/review");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "description: Review code\n\nLead with findings.",
        )
        .unwrap();
        let session = SessionStore::open(&workspace, SessionOpenMode::New).unwrap();
        let mut app = App::build(false, Some(session));
        app.tools = ToolRuntime::new(&workspace).unwrap();

        app.input = "/skills".to_string();
        app.input_cursor = app.input_len();
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(app.status_line, "skills listed");
        assert!(matches!(
            &app.transcript[..],
            [TranscriptItem::Message(ChatMessage { role: ChatRole::System, content, .. })]
                if content.contains("$review") && content.contains("Review code")
        ));
    }

    #[test]
    fn agents_command_opens_modal_with_workspace_agents() {
        let workspace = temp_workspace();
        let dir = workspace.join(".medusa/agents");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("reviewer.md"),
            "name: reviewer\ndescription: Review diffs\ntools: read\n\nAlways lead with findings.",
        )
        .unwrap();
        let session = SessionStore::open(&workspace, SessionOpenMode::New).unwrap();
        let mut app = App::build(false, Some(session));
        app.tools = ToolRuntime::new(&workspace).unwrap();

        app.input = "/agents".to_string();
        app.input_cursor = app.input_len();
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(app.active_modal, Some(Modal::Agents));
        assert_eq!(app.status_line, "named agents");
        assert_eq!(app.agent_registry.agents().len(), 1);
        assert_eq!(app.agent_registry.agents()[0].name, "reviewer");
        assert_eq!(
            app.agent_registry.agents()[0].tool_policy,
            SubagentToolPolicy::ReadOnly
        );

        // Esc closes through the generic modal fallback.
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.active_modal, None);
    }

    #[test]
    fn agents_command_shows_empty_state_when_unconfigured() {
        let mut app = app();

        app.input = "/agents".to_string();
        app.input_cursor = app.input_len();
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(app.active_modal, Some(Modal::Agents));
        assert!(app.agent_registry.is_empty());
    }

    #[test]
    fn slash_prefix_suggests_mcp_command() {
        let mut app = app();

        app.input = "/mc".to_string();
        app.input_cursor = 3;

        let matches = app.slash_matches();
        assert!(matches.iter().any(|(command, _)| command.name == "/mcp"));
    }

    #[test]
    fn mcp_command_opens_modal_with_config_hint_when_unconfigured() {
        let mut app = app();

        app.input = "/mcp".to_string();
        app.input_cursor = app.input_len();
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(app.active_modal, Some(Modal::Mcp));
        assert!(app.mcp_statuses.is_empty());
        assert_eq!(app.status_line, "mcp servers");

        // Esc closes through the generic modal fallback.
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.active_modal, None);
    }

    #[test]
    fn mcp_command_snapshots_configured_servers_without_starting_them() {
        let workspace = temp_workspace();
        fs::create_dir_all(workspace.join(".medusa")).unwrap();
        fs::write(
            workspace.join(".medusa/mcp.json"),
            r#"{"servers":{"docs":{"command":"python3","args":["server.py"],"readOnly":true}}}"#,
        )
        .unwrap();
        let session = SessionStore::open(&workspace, SessionOpenMode::New).unwrap();
        let mut app = App::build(false, Some(session));
        app.mcp = McpRegistry::load(&workspace).unwrap();
        app.tools = ToolRuntime::new(&workspace)
            .unwrap()
            .with_mcp(app.mcp.clone());

        app.input = "/mcp".to_string();
        app.input_cursor = app.input_len();
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(app.active_modal, Some(Modal::Mcp));
        assert_eq!(app.mcp_statuses.len(), 1);
        assert_eq!(app.mcp_statuses[0].name, "docs");
        assert_eq!(app.mcp_statuses[0].state, McpServerStateLabel::Idle);
        assert!(app.mcp_statuses[0].read_only);
        assert_eq!(app.mcp_statuses[0].command_line, "python3 server.py");
    }

    #[test]
    fn mcp_restart_validates_the_server_name() {
        let mut app = app();

        app.input = "/mcp restart nope".to_string();
        app.input_cursor = app.input_len();
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(app.status_line, "unknown mcp server");
        assert_eq!(app.active_modal, None);

        app.input = "/mcp bogus".to_string();
        app.input_cursor = app.input_len();
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(app.status_line, "usage: /mcp [restart <server>]");
    }

    #[test]
    fn unknown_slash_command_does_not_hit_model() {
        let mut app = app();

        app.input = "/nope".to_string();
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(app.status_line, "unknown command");
        assert!(matches!(
            &app.transcript[..],
            [TranscriptItem::Message(ChatMessage { role: ChatRole::System, content, .. })]
                if content.contains("unknown command: /nope")
        ));
    }

    #[test]
    fn plan_mode_badge_shows_in_composer_title() {
        let mut app = app();

        app.plan_mode = true;
        let title = app.input_title_content();
        let text = title
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(text.contains(" plan "));
    }

    #[test]
    fn workflow_events_create_and_finish_transcript_run() {
        let mut app = app();
        let phases = vec![WorkflowPhasePlan {
            name: "recon".to_string(),
            objective: "Map code".to_string(),
            agents: vec![SubagentSpec {
                name: "mapper".to_string(),
                role: "mapper".to_string(),
                prompt: "inspect".to_string(),
                allow_mutation: false,
                tool_policy: SubagentToolPolicy::ReadOnly,
            }],
        }];

        app.apply_workflow_event(WorkflowEvent::RunStarted {
            run_id: "workflow-test".to_string(),
            title: "inspect code".to_string(),
            task: "inspect code".to_string(),
            phases,
        });
        app.apply_workflow_event(WorkflowEvent::AgentStarted {
            run_id: "workflow-test".to_string(),
            phase_index: 0,
            agent_index: 0,
            name: "mapper".to_string(),
            role: "mapper".to_string(),
            tool_policy: SubagentToolPolicy::ShellRead,
        });
        app.apply_workflow_event(WorkflowEvent::AgentFinished {
            run_id: "workflow-test".to_string(),
            phase_index: 0,
            agent_index: 0,
            name: "mapper".to_string(),
            status: WorkflowStatus::Succeeded,
            output: "found src/main.rs".to_string(),
            tool_counts: BTreeMap::new(),
        });
        let finished = app.apply_workflow_event(WorkflowEvent::RunFinished {
            run_id: "workflow-test".to_string(),
            status: WorkflowStatus::Succeeded,
            summary: "workflow completed".to_string(),
        });

        assert!(finished);
        assert_eq!(app.workflows.len(), 1);
        assert_eq!(app.workflows[0].status, WorkflowViewState::Succeeded);
        assert!(matches!(
            app.transcript.first(),
            Some(TranscriptItem::Workflow(workflow))
                if workflow.id == "workflow-test" && workflow.summary == "workflow completed"
        ));
        assert!(matches!(
            app.transcript.last(),
            Some(TranscriptItem::Message(ChatMessage { role: ChatRole::Assistant, content, .. }))
                if content == "workflow completed"
        ));
    }

    #[test]
    fn script_workflow_events_append_dynamic_phases_and_agents() {
        let mut app = app();

        app.apply_workflow_event(WorkflowEvent::RunStarted {
            run_id: "script-test".to_string(),
            title: "script:bug-hunt".to_string(),
            task: "bug-hunt".to_string(),
            phases: Vec::new(),
        });
        app.apply_workflow_event(WorkflowEvent::PhaseStarted {
            run_id: "script-test".to_string(),
            phase_index: 0,
            name: "find round 1".to_string(),
            agent_count: 0,
        });
        app.apply_workflow_event(WorkflowEvent::AgentStarted {
            run_id: "script-test".to_string(),
            phase_index: 0,
            agent_index: 1,
            name: "finder-2".to_string(),
            role: "finder-2".to_string(),
            tool_policy: SubagentToolPolicy::ShellRead,
        });
        app.apply_workflow_event(WorkflowEvent::AgentFinished {
            run_id: "script-test".to_string(),
            phase_index: 0,
            agent_index: 1,
            name: "finder-2".to_string(),
            status: WorkflowStatus::Succeeded,
            output: "no bugs".to_string(),
            tool_counts: BTreeMap::new(),
        });
        app.apply_workflow_event(WorkflowEvent::Log {
            run_id: "script-test".to_string(),
            message: "round 1: nothing new".to_string(),
        });

        let workflow = &app.workflows[0];
        assert_eq!(workflow.phases.len(), 1);
        assert_eq!(workflow.phases[0].name, "find round 1");
        assert_eq!(workflow.phases[0].agents.len(), 2);
        assert_eq!(workflow.phases[0].agents[1].name, "finder-2");
        assert_eq!(
            workflow.phases[0].agents[1].status,
            WorkflowViewState::Succeeded
        );
        assert!(app.status_line.contains("round 1: nothing new"));
    }

    #[test]
    fn partial_workflow_is_not_rendered_as_total_failure() {
        let mut app = app();
        let phases = vec![
            WorkflowPhasePlan {
                name: "implementation".to_string(),
                objective: "Make change".to_string(),
                agents: vec![SubagentSpec {
                    name: "implementer".to_string(),
                    role: "implementation agent".to_string(),
                    prompt: "edit".to_string(),
                    allow_mutation: true,
                    tool_policy: SubagentToolPolicy::Edit,
                }],
            },
            WorkflowPhasePlan {
                name: "verification".to_string(),
                objective: "Verify".to_string(),
                agents: vec![SubagentSpec {
                    name: "verifier".to_string(),
                    role: "verification agent".to_string(),
                    prompt: "verify".to_string(),
                    allow_mutation: false,
                    tool_policy: SubagentToolPolicy::Verify,
                }],
            },
        ];

        app.apply_workflow_event(WorkflowEvent::RunStarted {
            run_id: "workflow-partial".to_string(),
            title: "split tui crate".to_string(),
            task: "split tui crate".to_string(),
            phases,
        });
        app.apply_workflow_event(WorkflowEvent::AgentFinished {
            run_id: "workflow-partial".to_string(),
            phase_index: 0,
            agent_index: 0,
            name: "implementer".to_string(),
            status: WorkflowStatus::Succeeded,
            output: "moved terminal helpers".to_string(),
            tool_counts: BTreeMap::new(),
        });
        app.apply_workflow_event(WorkflowEvent::AgentFinished {
            run_id: "workflow-partial".to_string(),
            phase_index: 1,
            agent_index: 0,
            name: "verifier".to_string(),
            status: WorkflowStatus::Failed,
            output: "subagent failed: backend overloaded".to_string(),
            tool_counts: BTreeMap::new(),
        });
        let finished = app.apply_workflow_event(WorkflowEvent::RunFinished {
            run_id: "workflow-partial".to_string(),
            status: WorkflowStatus::PartiallySucceeded,
            summary: "workflow partially completed: useful work landed; verifier failed"
                .to_string(),
        });

        assert!(finished);
        assert_eq!(
            app.workflows[0].status,
            WorkflowViewState::PartiallySucceeded
        );
        assert_eq!(app.status_line, "workflow partially complete");
        assert_eq!(
            app.toast.as_ref().map(|toast| toast.kind),
            Some(ToastKind::Warning)
        );
        assert!(matches!(
            app.transcript.last(),
            Some(TranscriptItem::Message(ChatMessage { role: ChatRole::Assistant, content, .. }))
                if content.contains("partially completed")
        ));
    }

    #[test]
    fn active_workflow_does_not_block_model_turn_but_blocks_reload() {
        let mut app = App::with_model_backend(true);
        let (_sender, receiver) = mpsc::channel();
        let workflow = background_workflow(&app, receiver);
        app.workflow_events.push(workflow);

        assert!(!app.is_working());
        assert!(app.has_active_workflows());

        app.start_model_turn("foreground task");
        assert!(app.model_events.is_some());
        assert!(app.queued_turns.is_empty());

        app.model_events = None;
        app.streaming_message = None;
        app.request_reload();

        assert!(!app.should_quit);
        assert_eq!(app.status_line, "reload blocked: work is still running");
    }

    /// Finding [23]: the real turn-start path must hand the worker a
    /// `ToolRuntime` carrying the checkpoint recorder AND the turn's cancel
    /// token — deleting any of that wiring must fail this test.
    #[test]
    fn start_model_turn_wires_recorder_and_cancel_onto_worker_runtime() {
        let mut app = App::with_model_backend(true);
        app.transcript
            .push(TranscriptItem::Message(ChatMessage::user("edit the file")));

        app.start_model_turn("edit the file");

        // App-side wiring (guards `self.active_checkpoint` / `self.turn_cancel`).
        assert!(
            app.active_checkpoint.is_some(),
            "turn must own a checkpoint recorder for finish/prune"
        );
        let turn_cancel = app
            .turn_cancel
            .clone()
            .expect("turn must own a cancel token");

        // Worker-side wiring: the exact runtime handed to the worker must carry
        // the recorder and the SAME cancel token (App-side state cannot prove
        // the `.with_checkpoint_recorder` / `.with_cancel_token` calls).
        let runtime = app
            .last_turn_runtime
            .as_ref()
            .expect("worker runtime should be captured");
        assert!(
            runtime.has_checkpoint_recorder(),
            "worker runtime must carry the checkpoint recorder"
        );
        assert!(!runtime.cancel_token().is_cancelled());
        turn_cancel.cancel();
        assert!(
            runtime.cancel_token().is_cancelled(),
            "worker runtime must share the turn's cancel token"
        );
    }

    /// Finding [12]: background workflow tools must carry a checkpoint recorder
    /// (and a cancel token) so subagent file edits are rewindable.
    #[test]
    fn start_workflow_wires_recorder_and_cancel_onto_worker_runtime() {
        let mut app = App::with_model_backend(true);

        app.start_workflow("refactor auth");

        let runtime = app
            .last_workflow_runtime
            .as_ref()
            .expect("workflow worker runtime should be captured");
        assert!(
            runtime.has_checkpoint_recorder(),
            "workflow subagent edits must be checkpointed so /rewind can undo them"
        );
        let workflow = app.workflow_events.last().expect("workflow registered");
        assert!(!workflow.cancel.is_cancelled());
        // A turn cancel stops the background workflow's tools.
        app.finalize_cancelled_turn("interrupted");
        assert!(
            app.workflow_events.iter().all(|w| w.cancel.is_cancelled()),
            "cancel must reach the background workflow so its tools bail"
        );
    }

    /// Finding [12] end-to-end: a file mutated during a workflow run lands in a
    /// checkpoint that `/rewind` can list and restore.
    #[test]
    fn workflow_file_edits_are_captured_in_a_rewindable_checkpoint() {
        let mut app = App::with_model_backend(true);
        let workspace = app.tools.workspace().to_path_buf();
        fs::write(workspace.join("auth.rs"), "old\n").unwrap();

        app.start_workflow("refactor auth");
        // A subagent edits a file through the run's shared recorder (the
        // worker's ToolRuntime holds a clone of this exact recorder).
        let recorder = app.workflow_events.last().unwrap().checkpoint.clone();
        recorder.capture(&["auth.rs".to_string()]).unwrap();
        fs::write(workspace.join("auth.rs"), "new\n").unwrap();

        let entries = CheckpointStore::open(&workspace)
            .and_then(|store| store.list())
            .unwrap();
        let entry = entries
            .iter()
            .find(|entry| entry.prompt_excerpt == "/workflow refactor auth")
            .expect("workflow run must produce a rewindable checkpoint");

        // And it actually restores the pre-edit content.
        CheckpointStore::open(&workspace)
            .and_then(|store| store.restore(&entry.id))
            .unwrap();
        assert_eq!(
            fs::read_to_string(workspace.join("auth.rs")).unwrap(),
            "old\n"
        );
    }

    #[test]
    fn drain_workflow_events_keeps_other_background_jobs_active() {
        let mut app = app();
        let phases = vec![WorkflowPhasePlan {
            name: "recon".to_string(),
            objective: "Map code".to_string(),
            agents: Vec::new(),
        }];
        let (finished_sender, finished_receiver) = mpsc::channel();
        let (_active_sender, active_receiver) = mpsc::channel();
        finished_sender
            .send(WorkflowEvent::RunStarted {
                run_id: "workflow-test".to_string(),
                title: "inspect code".to_string(),
                task: "inspect code".to_string(),
                phases,
            })
            .unwrap();
        finished_sender
            .send(WorkflowEvent::RunFinished {
                run_id: "workflow-test".to_string(),
                status: WorkflowStatus::Succeeded,
                summary: "workflow completed".to_string(),
            })
            .unwrap();
        drop(finished_sender);
        let finished = background_workflow(&app, finished_receiver);
        let active = background_workflow(&app, active_receiver);
        app.workflow_events.push(finished);
        app.workflow_events.push(active);

        app.drain_workflow_events();

        assert_eq!(app.workflow_events.len(), 1);
        assert_eq!(app.workflows.len(), 1);
        assert_eq!(app.workflows[0].status, WorkflowViewState::Succeeded);
        assert!(matches!(
            app.transcript.last(),
            Some(TranscriptItem::Message(ChatMessage { role: ChatRole::Assistant, content, .. }))
                if content == "workflow completed"
        ));
    }

    /// [7]: a turn queued while a background workflow runs must start when the
    /// workflow finishes — the only other dequeue site is a model turn's
    /// completion, so without this the prompt strands forever.
    #[test]
    fn finished_background_workflow_starts_queued_turn() {
        let mut app = app(); // model disabled: no worker thread, fully offline
        let (sender, receiver) = mpsc::channel::<WorkflowEvent>();
        app.workflow_events
            .push(background_workflow(&app, receiver));
        app.queued_turns
            .push_back("fix the failing test".to_string());

        // The workflow completes and its channel disconnects.
        sender
            .send(WorkflowEvent::RunFinished {
                run_id: "workflow-test".to_string(),
                status: WorkflowStatus::Succeeded,
                summary: "done".to_string(),
            })
            .unwrap();
        drop(sender);

        app.drain_workflow_events();

        assert!(
            app.queued_turns.is_empty(),
            "workflow completion must drain the queued turn"
        );
        assert!(
            app.transcript.iter().any(|item| matches!(
                item,
                TranscriptItem::Message(ChatMessage { role: ChatRole::User, content, .. })
                    if content == "fix the failing test"
            )),
            "the queued turn must actually be started (its user message appended)"
        );
    }

    #[test]
    fn workflow_rows_render_phase_tree() {
        let workflow = WorkflowRunView {
            id: "workflow-test".to_string(),
            title: "audit auth".to_string(),
            task: "audit auth".to_string(),
            status: WorkflowViewState::Running,
            phases: vec![WorkflowPhaseView {
                name: "recon".to_string(),
                objective: "Map code".to_string(),
                status: WorkflowViewState::Running,
                agents: vec![WorkflowAgentView {
                    name: "mapper".to_string(),
                    role: "mapper".to_string(),
                    tool_policy: SubagentToolPolicy::ShellRead,
                    status: WorkflowViewState::Running,
                    output: String::new(),
                    tool_counts: BTreeMap::new(),
                }],
            }],
            summary: String::new(),
            expanded: false,
        };
        let transcript = vec![TranscriptItem::Workflow(workflow)];

        let lines = visible_transcript_lines(&transcript, None, None);
        let text = lines.iter().map(line_text).collect::<Vec<_>>();

        assert!(text.iter().any(|line| line.contains("workflow")));
        assert!(text.iter().any(|line| line.contains("audit auth")));
        assert!(text.iter().any(|line| line.contains("recon")));
        assert!(text.iter().any(|line| line.contains("mapper")));
        assert!(text.iter().any(|line| line.contains("[shell-read]")));
        assert!(text.iter().any(|line| line.contains("running")));
    }

    #[test]
    fn empty_transcript_renders_launch_masthead() {
        let transcript = Vec::new();
        let lines = visible_transcript_lines(&transcript, None, None);
        let text = lines.iter().map(line_text).collect::<Vec<_>>();

        // Wordmark is width-adaptive: wide ansi-shadow or compact fallback.
        assert!(
            text.iter()
                .any(|line| line.contains("███╗") || line.contains("█▀▄▀█"))
        );
        assert!(
            text.iter()
                .any(|line| line.contains("plans, edits, and verifies"))
        );
        assert!(text.iter().any(|line| line.contains("shift+tab")));
        assert!(text.iter().any(|line| line.contains("ctrl+p")));
        assert!(text.iter().any(|line| line.contains("esc esc")));
    }

    #[test]
    fn first_pending_turn_keeps_launch_masthead_visible() {
        let transcript = vec![
            TranscriptItem::Message(ChatMessage::user("hi")),
            TranscriptItem::Message(ChatMessage::assistant("")),
        ];
        let lines = visible_transcript_lines(&transcript, Some(1), None);
        let text = lines.iter().map(line_text).collect::<Vec<_>>();

        assert!(
            text.iter()
                .any(|line| line.contains("plans, edits, and verifies"))
        );
        assert!(text.iter().any(|line| line.contains("› hi")));
    }

    #[test]
    fn toast_renders_in_status_line() {
        let mut app = app();

        app.toast("Session cleared", ToastKind::Warning);

        let text = line_text(&app.status_line_content());
        assert!(text.contains("warning"));
        assert!(text.contains("Session cleared"));
    }

    #[test]
    fn tree_command_opens_session_tree_modal() {
        let mut app = app();

        app.input = "/tree".to_string();
        app.input_cursor = app.input_len();
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(app.status_line, "session tree opened");
        assert_eq!(app.active_modal, Some(Modal::SessionTree));
    }

    #[test]
    fn session_fork_writes_parent_metadata_and_updates_pointer() {
        let workspace = temp_workspace();
        let mut session = SessionStore::open(&workspace, SessionOpenMode::New).unwrap();
        let root_id = session.current_id();
        let transcript = vec![TranscriptItem::Message(ChatMessage::user("root task"))];

        session.save_transcript(&transcript).unwrap();
        let fork_id = session.fork(&transcript).unwrap();

        assert_ne!(fork_id, root_id);
        assert_eq!(
            fs::read_to_string(workspace.join(".medusa/sessions/last")).unwrap(),
            fork_id
        );

        let fork_file = read_session_file::<TranscriptItem, ChatMessage>(
            &workspace.join(".medusa/sessions").join(&fork_id),
        )
        .expect("fork session file should parse");
        assert_eq!(fork_file.session_id.as_deref(), Some(fork_id.as_str()));
        assert_eq!(fork_file.parent_id.as_deref(), Some(root_id.as_str()));
        assert_eq!(fork_file.transcript, transcript);

        let sessions = session.list_sessions();
        let fork = sessions
            .iter()
            .find(|info| info.name == fork_id)
            .expect("fork should be listed");
        assert_eq!(fork.parent, compact_session_id(&root_id));
        assert_eq!(fork.current, "yes");
    }

    #[test]
    fn startup_parser_accepts_named_continue() {
        let args = vec!["continue".to_string(), "session-123.json".to_string()];

        assert_eq!(
            parse_startup_command(&args).unwrap(),
            StartupCommand::Tui(SessionOpenMode::ContinueNamed(
                "session-123.json".to_string()
            ))
        );
    }

    #[test]
    fn startup_parser_accepts_headless_run_options() {
        let args = vec![
            "run".to_string(),
            "--model".to_string(),
            "gpt-test".to_string(),
            "--permission".to_string(),
            "readonly".to_string(),
            "--json".to_string(),
            "--".to_string(),
            "fix".to_string(),
            "tests".to_string(),
        ];

        assert_eq!(
            parse_startup_command(&args).unwrap(),
            StartupCommand::Headless(HeadlessOptions {
                task: Some("fix tests".to_string()),
                model: Some("gpt-test".to_string()),
                permission_mode: Some(PermissionMode::Readonly),
                json: true,
                stream: false,
            })
        );
    }

    #[test]
    fn startup_parser_allows_headless_run_task_from_stdin() {
        let args = vec!["run".to_string(), "--no-stream".to_string()];

        assert_eq!(
            parse_startup_command(&args).unwrap(),
            StartupCommand::Headless(HeadlessOptions {
                task: None,
                model: None,
                permission_mode: None,
                json: false,
                stream: false,
            })
        );
    }

    #[test]
    fn session_name_rejects_traversal() {
        assert!(normalize_session_name("../session-1").is_err());
        assert!(normalize_session_name("nested/session-1").is_err());
        assert!(normalize_session_name("last").is_err());
    }

    #[test]
    fn named_session_open_loads_requested_transcript() {
        let workspace = temp_workspace();
        let mut session = SessionStore::open(&workspace, SessionOpenMode::New).unwrap();
        let root_id = session.current_id();
        let root_transcript = vec![TranscriptItem::Message(ChatMessage::user("root task"))];
        let fork_transcript = vec![TranscriptItem::Message(ChatMessage::user("fork task"))];

        session.save_transcript(&root_transcript).unwrap();
        let fork_id = session.fork(&fork_transcript).unwrap();
        assert_ne!(fork_id, root_id);

        let named = SessionStore::open(
            &workspace,
            SessionOpenMode::ContinueNamed(root_id.trim_end_matches(".json").to_string()),
        )
        .unwrap();

        assert_eq!(named.current_id(), root_id);
        assert_eq!(named.load_transcript().unwrap(), root_transcript);
        assert_eq!(
            fs::read_to_string(workspace.join(".medusa/sessions/last")).unwrap(),
            root_id
        );
    }

    #[test]
    fn fork_command_switches_active_session() {
        let workspace = temp_workspace();
        let session = SessionStore::open(&workspace, SessionOpenMode::New).unwrap();
        let mut app = App::build(false, Some(session));
        app.transcript
            .push(TranscriptItem::Message(ChatMessage::user("try one path")));

        app.input = "/fork".to_string();
        app.input_cursor = app.input_len();
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(app.status_line.starts_with("forked session-"));
        assert!(app.session.as_ref().unwrap().parent_id().is_some());
        assert_eq!(app.transcript.len(), 1);
    }

    #[test]
    fn resume_command_switches_active_session() {
        let workspace = temp_workspace();
        let mut session = SessionStore::open(&workspace, SessionOpenMode::New).unwrap();
        let root_id = session.current_id();
        let root_transcript = vec![TranscriptItem::Message(ChatMessage::user("root task"))];
        let fork_transcript = vec![TranscriptItem::Message(ChatMessage::user("fork task"))];

        session.save_transcript(&root_transcript).unwrap();
        session.fork(&fork_transcript).unwrap();
        let mut app = App::build(false, Some(session));
        app.transcript = fork_transcript;

        app.input = format!("/resume {}", root_id.trim_end_matches(".json"));
        app.input_cursor = app.input_len();
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(app.status_line, format!("resumed {root_id}"));
        assert_eq!(app.transcript, root_transcript);
        assert_eq!(app.session.as_ref().unwrap().current_id(), root_id);
        assert_eq!(
            fs::read_to_string(workspace.join(".medusa/sessions/last")).unwrap(),
            root_id
        );
    }

    #[test]
    fn input_title_shows_model_name_when_idle() {
        let app = app();

        let title = app.input_title_content();
        let text = title
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(text.contains(app.model.model_name()));
        assert!(!text.contains("━"));
    }

    #[test]
    fn input_title_uses_light_sweep_while_working() {
        let mut app = app();

        app.animation_tick = 0;
        app.streaming_message = Some(0);
        let title = app.input_title_content();
        let text = title
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(text.contains("━"));
        assert!(!text.to_lowercase().contains("message"));
    }

    #[test]
    fn input_height_is_compact_but_allows_multiline_growth() {
        let mut app = app();

        assert_eq!(app.input_height(40), 3);

        app.input = "one\ntwo\nthree".to_string();
        assert_eq!(app.input_height(40), 5);
    }

    #[test]
    fn input_lines_are_vertically_centered_in_composer() {
        let lines = input_display_lines("", 0, 1);
        let centered = vertically_center_input_lines(lines, 1);

        assert_eq!(centered.len(), 1);
        assert!(
            centered[0]
                .spans
                .iter()
                .any(|span| span.content.contains("Type a task"))
        );
    }

    #[test]
    fn input_display_lines_only_renders_visible_tail_near_cursor() {
        let input = (0..1000)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n");

        let lines = input_display_lines(&input, input.chars().count(), 3);
        let rendered = lines.iter().map(line_text).collect::<Vec<_>>();

        assert_eq!(lines.len(), 3);
        assert!(rendered[0].contains("line 997"));
        assert!(rendered[1].contains("line 998"));
        assert!(rendered[2].contains("line 999"));
    }

    #[test]
    fn large_paste_inserts_in_one_batch() {
        let mut app = app();
        let paste = (0..1000)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n");

        app.handle_paste(paste.clone());

        assert_eq!(app.input, paste);
        assert_eq!(app.input_cursor, paste.chars().count());
    }

    #[test]
    fn page_up_and_down_scroll_chat() {
        let mut app = app();

        app.handle_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
        assert_eq!(app.chat_scroll_target, 12);
        assert_eq!(app.chat_scroll, 12);

        app.handle_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
        assert_eq!(app.chat_scroll_target, 0);
        assert_eq!(app.chat_scroll, 0);
    }

    #[test]
    fn page_scroll_uses_chat_viewport_height_when_known() {
        let mut app = scrollback_app(40, Rect::new(0, 0, 80, 10));

        app.handle_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
        assert_eq!(app.chat_scroll_target, 8);
        assert_eq!(app.chat_scroll, 8);

        app.handle_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
        assert_eq!(app.chat_scroll_target, 0);
        assert_eq!(app.chat_scroll, 0);
    }

    #[test]
    fn mouse_wheel_uses_expected_scroll_amounts() {
        let app = scrollback_app(40, Rect::new(0, 0, 80, 10));

        assert_eq!(
            app.mouse_scroll_amount(wheel_event(MouseEventKind::ScrollUp, KeyModifiers::NONE)),
            6
        );
        assert_eq!(
            app.mouse_scroll_amount(wheel_event(MouseEventKind::ScrollUp, KeyModifiers::CONTROL)),
            1
        );
        assert_eq!(
            app.mouse_scroll_amount(wheel_event(MouseEventKind::ScrollUp, KeyModifiers::SHIFT)),
            app.chat_page_scroll_amount()
        );
        assert_eq!(app.chat_page_scroll_amount(), 8);
    }

    #[test]
    fn wheel_events_request_immediate_draw() {
        assert!(event_requests_immediate_draw(&Event::Mouse(wheel_event(
            MouseEventKind::ScrollUp,
            KeyModifiers::NONE
        ))));
        assert!(event_requests_immediate_draw(&Event::Mouse(wheel_event(
            MouseEventKind::ScrollDown,
            KeyModifiers::NONE
        ))));
        assert!(!event_requests_immediate_draw(&Event::Key(KeyEvent::new(
            KeyCode::Char('a'),
            KeyModifiers::NONE
        ))));
    }

    #[test]
    fn ctrl_home_scrolls_to_oldest_visible_content() {
        let mut app = app();
        app.transcript = (0..40)
            .map(|index| TranscriptItem::Message(ChatMessage::assistant(format!("line {index}"))))
            .collect();
        app.last_chat_viewport = Some(Rect::new(0, 0, 80, 10));

        app.handle_key(KeyEvent::new(KeyCode::Home, KeyModifiers::CONTROL));

        assert_eq!(app.chat_scroll, 31);
        assert_eq!(app.chat_scroll_target, 31);
        assert_eq!(app.status_line, "top");
    }

    #[test]
    fn viewport_metrics_anchor_bottom_and_clamp_top() {
        let rows = (0..20)
            .map(|index| TranscriptRow::text(Line::from(format!("line {index}"))))
            .collect::<Vec<_>>();
        let area = Rect::new(0, 0, 80, 5);

        let bottom = chat_viewport_metrics(&rows, area, 0);
        assert_eq!(bottom.max_scroll, 15);
        assert_eq!(bottom.top_offset, 15);

        let middle = chat_viewport_metrics(&rows, area, 10);
        assert_eq!(middle.scroll, 10);
        assert_eq!(middle.top_offset, 5);

        let top = chat_viewport_metrics(&rows, area, usize::MAX);
        assert_eq!(top.scroll, 15);
        assert_eq!(top.top_offset, 0);
    }

    #[test]
    fn user_message_rows_have_background_style() {
        let transcript = vec![TranscriptItem::Message(ChatMessage::user("hello"))];

        let rows = visible_transcript_rows(&transcript, None, None, RenderContext::static_view());
        let user_row = &rows[0].line;

        assert_eq!(line_text(user_row), " › hello ");
        let expected_bg = user_row.spans.first().and_then(|span| span.style.bg);
        assert!(expected_bg.is_some());
        assert!(
            user_row
                .spans
                .iter()
                .all(|span| span.style.bg == expected_bg)
        );
    }

    #[test]
    fn visible_transcript_rows_include_bottom_padding() {
        let transcript = vec![TranscriptItem::Message(ChatMessage::assistant("done"))];

        let rows = visible_transcript_rows(&transcript, None, None, RenderContext::static_view());

        assert_eq!(line_text(&rows.last().unwrap().line), "");
    }

    #[test]
    fn viewport_bottom_anchor_leaves_padding_below_last_message() {
        let rows = (0..5)
            .map(|index| TranscriptRow::text(Line::from(format!("line {index}"))))
            .chain(std::iter::once(TranscriptRow::text(Line::from(""))))
            .collect::<Vec<_>>();
        let area = Rect::new(0, 0, 80, 5);

        let metrics = chat_viewport_metrics(&rows, area, 0);

        assert_eq!(metrics.top_offset, 1);
        assert_eq!(metrics.max_scroll, 1);
    }

    #[test]
    fn viewport_metrics_count_wrapped_visual_lines() {
        let rows = vec![TranscriptRow::text(Line::from("abcdefghijklmnopqrst"))];
        let metrics = chat_viewport_metrics(&rows, Rect::new(0, 0, 10, 1), 0);

        assert_eq!(metrics.total_visual_lines, 2);
        assert_eq!(metrics.max_scroll, 1);
        assert_eq!(metrics.top_offset, 1);
    }

    #[test]
    fn viewport_metrics_keep_text_width_stable_when_overflowing() {
        let rows = (0..20)
            .map(|index| TranscriptRow::text(Line::from(format!("line {index}"))))
            .collect::<Vec<_>>();
        let area = Rect::new(0, 0, 12, 4);

        let metrics = chat_viewport_metrics(&rows, area, 0);

        assert!(metrics.has_scrollbar);
        assert_eq!(metrics.text_area.width, area.width);
    }

    #[test]
    fn scroll_status_reports_position() {
        let mut app = app();
        app.transcript = (0..40)
            .map(|index| TranscriptItem::Message(ChatMessage::assistant(format!("line {index}"))))
            .collect();
        app.last_chat_viewport = Some(Rect::new(0, 0, 80, 10));

        app.scroll_chat_up(8);

        assert!(app.status_line.starts_with("scroll "));
    }

    #[test]
    fn wheel_scroll_updates_visible_offset_immediately() {
        let mut app = scrollback_app(40, Rect::new(0, 0, 80, 10));

        let before = app.current_chat_viewport_metrics().unwrap();
        assert_eq!(before.top_offset, 31);

        app.handle_mouse(wheel_event(MouseEventKind::ScrollUp, KeyModifiers::NONE));

        assert_eq!(app.chat_scroll, 6);
        assert_eq!(app.chat_scroll_target, 6);
        let after = app.current_chat_viewport_metrics().unwrap();
        assert_eq!(after.top_offset, 25);
    }

    #[test]
    fn repeated_wheel_scroll_does_not_build_hidden_scroll_debt() {
        let mut app = scrollback_app(200, Rect::new(0, 0, 80, 10));

        for _ in 0..40 {
            app.handle_mouse(wheel_event(MouseEventKind::ScrollUp, KeyModifiers::NONE));
        }

        assert_eq!(app.chat_scroll, app.chat_scroll_target);
        assert_eq!(
            app.chat_scroll,
            app.current_chat_viewport_metrics().unwrap().max_scroll
        );

        app.handle_mouse(wheel_event(MouseEventKind::ScrollDown, KeyModifiers::NONE));

        assert_eq!(app.chat_scroll, app.chat_scroll_target);
        assert!(app.chat_scroll < app.current_chat_viewport_metrics().unwrap().max_scroll);
    }

    #[test]
    fn viewport_trimmer_skips_each_row_once() {
        let rows = ["a", "b", "c", "d"]
            .into_iter()
            .map(|line| TranscriptRow::text(Line::from(line)))
            .collect::<Vec<_>>();
        let visible = trim_wrapped_lines_for_viewport(&rows, 80, 2, 2);
        let text = visible
            .iter()
            .map(|row| line_text(&row.line))
            .collect::<Vec<_>>();

        assert_eq!(text, vec!["c", "d"]);
    }

    #[test]
    fn ctrl_end_returns_to_bottom() {
        let mut app = app();

        app.chat_scroll = 42;
        app.chat_scroll_target = 42;
        app.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL));

        assert_eq!(app.chat_scroll, 0);
        assert_eq!(app.chat_scroll_target, 0);
    }

    #[test]
    fn workflow_updates_preserve_manual_scrollback() {
        let mut app = app();
        app.transcript = (0..40)
            .map(|index| TranscriptItem::Message(ChatMessage::assistant(format!("line {index}"))))
            .collect();
        app.last_chat_viewport = Some(Rect::new(0, 0, 80, 10));
        app.chat_scroll = 12;
        app.chat_scroll_target = 12;

        app.apply_workflow_event(WorkflowEvent::RunStarted {
            run_id: "run-1".to_string(),
            title: "Build".to_string(),
            task: "task".to_string(),
            phases: Vec::new(),
        });

        assert_eq!(app.chat_scroll, 12);
    }

    #[test]
    fn home_abbreviation_uses_tilde() {
        let home = env::var("HOME").unwrap();
        let nested = format!("{home}/code/project");

        assert_eq!(abbreviate_home(&nested), "~/code/project");
    }

    #[test]
    fn visible_chat_lines_distinguishes_roles() {
        let transcript = vec![
            TranscriptItem::Message(ChatMessage::user("hello")),
            TranscriptItem::Message(ChatMessage::assistant("hi")),
        ];
        let lines = visible_transcript_lines(&transcript, None, None);

        assert_eq!(lines.len(), 3 + CHAT_BOTTOM_PADDING_ROWS);
        let text = lines.iter().map(line_text).collect::<Vec<_>>();
        assert!(text.iter().any(|line| line.contains("hello")));
        assert!(text.iter().any(|line| line.contains("hi")));
    }

    #[test]
    fn reasoning_does_not_render_as_ghost_text() {
        let transcript = vec![
            TranscriptItem::Message(ChatMessage::user("read code")),
            TranscriptItem::Message(ChatMessage::assistant("The render loop is in main.rs.")),
            TranscriptItem::Reasoning(ReasoningTrace {
                content: "Hidden model thinking.".to_string(),
                expanded: false,
            }),
        ];

        let lines = visible_transcript_lines(&transcript, None, None);
        let text = lines.iter().map(line_text).collect::<Vec<_>>();

        assert!(
            text.iter()
                .any(|line| line.contains("The render loop is in main.rs."))
        );
        assert!(
            !text
                .iter()
                .any(|line| line.contains("Hidden model thinking"))
        );
        assert!(!text.iter().any(|line| line.contains("thinking")));
    }

    #[test]
    fn reasoning_is_not_tool_activity_selection() {
        let mut app = app();
        app.transcript
            .push(TranscriptItem::Reasoning(ReasoningTrace {
                content: "Thinking through render order.".to_string(),
                expanded: false,
            }));

        app.select_next_tool();

        assert_eq!(app.selected_tool, None);
        assert_eq!(app.status_line, "no tool activity");
    }

    #[test]
    fn ansi_escape_codes_render_as_colored_spans() {
        let spans = ansi_detail_spans("\u{1b}[31merror\u{1b}[0m: something broke", muted());
        assert!(spans.len() >= 2);
        assert_eq!(spans[0].content.as_ref(), "error");
        assert_eq!(spans[0].style.fg, Some(Color::Red));
        // Unstyled remainder falls back to the muted body style.
        assert_eq!(spans.last().unwrap().style, muted());
    }

    #[test]
    fn rust_code_blocks_get_syntax_highlighting() {
        let lines = markdown_content_lines(
            "```rust\nfn main() { let x = \"hi\"; }\n```",
            ChatRole::Assistant,
        );
        assert_eq!(lines.len(), 1);
        // Border span + several differently-styled token spans.
        assert!(
            lines[0].spans.len() > 3,
            "expected token-level spans, got {:?}",
            lines[0].spans
        );
        let distinct_colors = lines[0]
            .spans
            .iter()
            .skip(1)
            .filter_map(|span| span.style.fg)
            .collect::<std::collections::HashSet<_>>();
        assert!(distinct_colors.len() > 1, "expected multiple token colors");
    }

    #[test]
    fn unknown_language_code_blocks_fall_back_to_plain_style() {
        let lines = markdown_content_lines("```notalanguage\nsome text\n```", ChatRole::Assistant);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].spans.len(), 2);
    }

    #[test]
    fn markdown_content_lines_formats_common_blocks() {
        let content = "# Title\n\n- item `code`\n```rust\nfn main() {}\n```\n> quote";

        let lines = markdown_content_lines(content, ChatRole::Assistant);
        let text = lines.iter().map(line_text).collect::<Vec<_>>();

        assert_eq!(lines.len(), 5);
        assert!(text.iter().any(|line| line.contains("Title")));
        assert!(text.iter().any(|line| line.contains("fn main()")));
        assert!(!text.iter().any(|line| line.contains("code rust")));
    }

    #[test]
    fn clean_model_error_hides_backend_json_payloads() {
        let raw = r#"Codex backend stream ended with response.failed: {"response":{"id":"resp_test","instructions":"You are Medusa, a terminal-native autonomous coding agent","error":{"code":"server_is_overloaded","message":"Our servers are currently overloaded. Please try again later."}}}"#;

        let cleaned = clean_model_error(raw);

        assert_eq!(
            cleaned,
            "model overloaded: Our servers are currently overloaded. Please try again later."
        );
        assert_eq!(model_error_status(&cleaned), "model overloaded");
        assert!(!cleaned.contains("{\"response\""));
        assert!(!cleaned.contains("instructions"));
    }

    #[test]
    fn model_tool_events_update_compact_tool_run() {
        let mut app = app();

        app.push_tool_start("terminal.exec".to_string(), "$ cargo test".to_string());
        for item in &mut app.transcript {
            if let TranscriptItem::Tool(run) = item {
                run.started_at = run
                    .started_at
                    .checked_sub(MIN_TOOL_PULSE_VISIBLE)
                    .unwrap_or(run.started_at);
            }
        }
        app.push_tool_result("terminal.exec", "exit: 0\nstdout:\nok".to_string());

        assert_eq!(app.transcript.len(), 1);
        let TranscriptItem::Tool(inline_run) = &app.transcript[0] else {
            panic!("expected inline tool run");
        };
        assert_eq!(inline_run.name, "terminal.exec");
        assert_eq!(inline_run.state, ToolRunState::Succeeded);
        assert_eq!(inline_run.detail, "ok");
    }

    #[test]
    fn plan_command_toggles_plan_mode() {
        let mut app = app();
        assert!(!app.plan_mode);

        assert!(app.run_local_tool_command("/plan"));
        assert!(app.plan_mode);
        assert!(app.status_line.contains("plan mode on"));
        let history = app.conversation_history();
        assert!(
            history.iter().any(|message| message.role == "system"
                && message.content.contains("Plan mode is active"))
        );

        assert!(app.run_local_tool_command("/plan"));
        assert!(!app.plan_mode);
        assert!(
            !app.conversation_history()
                .iter()
                .any(|message| message.content.contains("Plan mode is active"))
        );
    }

    #[test]
    fn shift_tab_toggles_plan_mode_in_composer() {
        let mut app = app();

        app.handle_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
        assert!(app.plan_mode);

        app.handle_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
        assert!(!app.plan_mode);
    }

    #[test]
    fn single_escape_never_quits_idle_composer() {
        let mut app = app();

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        assert!(!app.should_quit);
        assert_eq!(app.status_line, "press esc again to quit");
    }

    #[test]
    fn double_escape_quits_within_window() {
        let mut app = app();

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        assert!(app.should_quit);
    }

    #[test]
    fn stale_escape_does_not_count_toward_quit() {
        let mut app = app();

        app.last_escape_at = Instant::now().checked_sub(DOUBLE_ESCAPE_WINDOW * 2);
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        assert!(!app.should_quit);
        assert_eq!(app.status_line, "press esc again to quit");
    }

    #[test]
    fn escape_clears_input_before_arming_quit() {
        let mut app = app();
        app.input = "half-typed task".to_string();
        app.input_cursor = app.input_len();

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!app.should_quit);
        assert!(app.input.is_empty());
        assert_eq!(app.status_line, "input cleared");

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!app.should_quit);
    }

    #[test]
    fn escape_exits_plan_mode_before_arming_quit() {
        let mut app = app();
        app.plan_mode = true;

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!app.plan_mode);
        assert!(!app.should_quit);
        assert!(app.status_line.contains("plan mode off"));

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!app.should_quit);
        assert_eq!(app.status_line, "press esc again to quit");
    }

    #[test]
    fn escape_deselects_tool_before_arming_quit() {
        let mut app = app();
        app.transcript.push(TranscriptItem::Tool(ToolRun {
            id: None,
            started_at: Instant::now(),
            pending_result: None,
            name: "terminal.exec".to_string(),
            summary: "$ ls".to_string(),
            state: ToolRunState::Succeeded,
            detail: "done".to_string(),
            expanded: true,
            group_expanded: false,
        }));
        app.selected_tool = Some(0);

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        assert!(!app.should_quit);
        assert_eq!(app.selected_tool, None);
    }

    fn queue_approval(app: &mut App, command: &str) -> mpsc::Receiver<ApprovalDecision> {
        queue_approval_kind(app, command, false)
    }

    fn queue_approval_kind(
        app: &mut App,
        command: &str,
        sandbox_escalation: bool,
    ) -> mpsc::Receiver<ApprovalDecision> {
        let (respond, decision) = mpsc::channel();
        app.approval_queue.push_back(PendingApproval {
            request: ApprovalRequest {
                tool: ApprovalTool::TerminalExec,
                command: Some(command.to_string()),
                paths: Vec::new(),
                background: false,
                sandbox_escalation,
            },
            respond,
        });
        // Pretend the prompt has been visible past the grace window so the
        // decision keys act immediately in tests.
        app.approval_shown_at = Instant::now().checked_sub(APPROVAL_KEY_GRACE * 2);
        decision
    }

    #[test]
    fn approval_keys_resolve_and_unblock_worker() {
        let mut app = app();
        let decision = queue_approval(&mut app, "cargo build");

        app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));

        assert_eq!(decision.try_recv().unwrap(), ApprovalDecision::AllowOnce);
        assert!(app.approval_queue.is_empty());
        assert_eq!(app.status_line, "approved once");
    }

    #[test]
    fn approval_deny_remembers_command_for_turn() {
        let mut app = app();
        let first = queue_approval(&mut app, "touch scary.txt");
        app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert_eq!(first.try_recv().unwrap(), ApprovalDecision::Deny);

        // A verbatim retry auto-denies without prompting.
        assert_eq!(
            app.auto_approval_decision(&ApprovalRequest {
                tool: ApprovalTool::TerminalExec,
                command: Some("touch scary.txt".to_string()),
                paths: Vec::new(),
                background: false,
                sandbox_escalation: false,
            }),
            Some(ApprovalDecision::Deny)
        );
    }

    #[test]
    fn escape_denies_pending_approval_without_quitting() {
        let mut app = app();
        let decision = queue_approval(&mut app, "cargo build");

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        assert_eq!(decision.try_recv().unwrap(), ApprovalDecision::Deny);
        assert!(!app.should_quit);

        // The armed-quit state was reset: next Esc only arms.
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!app.should_quit);
    }

    fn running_tool_row(name: &str) -> TranscriptItem {
        TranscriptItem::Tool(ToolRun {
            id: Some("call_running".to_string()),
            started_at: Instant::now(),
            pending_result: None,
            name: name.to_string(),
            summary: format!("$ {name}"),
            state: ToolRunState::Running,
            detail: String::new(),
            expanded: false,
            group_expanded: false,
        })
    }

    /// App in the "worker streaming" state with an attached cancel token.
    fn working_app() -> (App, mpsc::Sender<ModelStreamEvent>, CancelToken) {
        let mut app = app();
        let (sender, receiver) = mpsc::channel::<ModelStreamEvent>();
        app.model_events = Some(receiver);
        let token = CancelToken::new();
        app.turn_cancel = Some(token.clone());
        app.turn_started_at = Some(Instant::now());
        (app, sender, token)
    }

    #[test]
    fn escape_while_working_requests_cancel_instead_of_arming_quit() {
        let (mut app, _sender, token) = working_app();

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        assert!(!app.should_quit);
        assert!(token.is_cancelled());
        assert!(app.cancel_requested_at.is_some());
        assert!(
            app.status_line.contains("cancelling"),
            "{}",
            app.status_line
        );
        // Cancelling must not arm double-esc quit, and the receiver stays
        // attached so the worker can still report Cancelled.
        assert!(app.last_escape_at.is_none());
        assert!(app.model_events.is_some());
    }

    #[test]
    fn second_escape_force_abandons_the_turn() {
        let (mut app, _sender, _token) = working_app();
        app.transcript.push(running_tool_row("terminal.exec"));

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        assert!(!app.should_quit);
        assert!(app.model_events.is_none());
        assert!(app.turn_cancel.is_none());
        assert!(app.cancel_requested_at.is_none());
        assert_eq!(app.status_line, "turn abandoned");
        assert!(matches!(
            &app.transcript[0],
            TranscriptItem::Tool(run)
                if run.state == ToolRunState::Failed && run.detail == "cancelled"
        ));

        // Idle again: the classic arm-then-quit gesture still works.
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!app.should_quit);
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.should_quit);
    }

    #[test]
    fn escape_clears_input_before_cancelling_a_working_turn() {
        let (mut app, _sender, token) = working_app();
        app.input = "half-typed follow-up".to_string();
        app.input_cursor = app.input_len();

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.input.is_empty());
        assert!(!token.is_cancelled());

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(token.is_cancelled());
        assert!(!app.should_quit);
    }

    #[test]
    fn request_cancel_denies_every_queued_approval() {
        let (mut app, _sender, token) = working_app();
        let first = queue_approval(&mut app, "cargo build");
        let second = queue_approval(&mut app, "cargo test");

        app.request_cancel_turn();

        assert!(token.is_cancelled());
        assert!(app.approval_queue.is_empty());
        assert_eq!(first.try_recv().unwrap(), ApprovalDecision::Deny);
        assert_eq!(second.try_recv().unwrap(), ApprovalDecision::Deny);
    }

    #[test]
    fn approvals_arriving_after_cancel_are_denied_and_unblock_the_worker() {
        let mut app = app();
        app.cancel_requested_at = Some(Instant::now());

        // A tool worker parked in the approval handler, its request racing
        // the cancel: the drain must answer it with Deny, never queue it.
        let handler = app.approval_handler.clone();
        let worker = thread::spawn(move || {
            handler(ApprovalRequest {
                tool: ApprovalTool::TerminalExec,
                command: Some("cargo build".to_string()),
                paths: Vec::new(),
                background: false,
                sandbox_escalation: false,
            })
        });

        let deadline = Instant::now() + Duration::from_secs(2);
        while !app.drain_approval_requests() {
            assert!(Instant::now() < deadline, "approval request never arrived");
            thread::sleep(Duration::from_millis(10));
        }

        assert!(app.approval_queue.is_empty());
        assert_eq!(worker.join().unwrap(), ApprovalDecision::Deny);
    }

    #[test]
    fn cancelled_event_finalizes_the_turn_and_preserves_partial_text() {
        let (mut app, sender, _token) = working_app();
        app.cancel_requested_at = Some(Instant::now());
        app.transcript
            .push(TranscriptItem::Message(ChatMessage::user("do the thing")));
        app.transcript
            .push(TranscriptItem::Message(ChatMessage::assistant(
                "partial answer",
            )));
        app.streaming_message = Some(1);
        app.transcript.push(running_tool_row("terminal.exec"));
        app.queued_turns.push_back("queued task".to_string());

        sender.send(ModelStreamEvent::Cancelled).unwrap();
        app.drain_model_events();

        assert!(app.model_events.is_none());
        assert!(app.turn_cancel.is_none());
        assert!(app.cancel_requested_at.is_none());
        assert!(app.streaming_message.is_none());
        // [21]: the acknowledged queued prompt is kept, not silently dropped.
        assert_eq!(
            app.queued_turns
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec!["queued task"],
        );
        assert_eq!(app.status_line, "turn interrupted");
        assert!(matches!(
            &app.transcript[2],
            TranscriptItem::Tool(run)
                if run.state == ToolRunState::Failed && run.detail == "cancelled"
        ));
        assert!(matches!(
            app.transcript.last(),
            Some(TranscriptItem::Message(message))
                if message.role == ChatRole::System && message.content == TURN_INTERRUPTED_NOTE
        ));

        // Resumability: history keeps the partial assistant text, gains the
        // interruption note, and the app is idle so a fresh turn (with a
        // fresh token) can start.
        let history = app.conversation_history();
        assert!(
            history
                .iter()
                .any(|message| message.role == "assistant" && message.content == "partial answer")
        );
        assert!(
            history
                .iter()
                .any(|message| message.role == "system"
                    && message.content == TURN_INTERRUPTED_NOTE)
        );
        assert!(!app.is_working());
    }

    #[test]
    fn error_event_while_cancelling_renders_as_interruption_not_failure() {
        let (mut app, sender, _token) = working_app();
        app.cancel_requested_at = Some(Instant::now());

        sender
            .send(ModelStreamEvent::Error(
                "failed to send stream event: receiving on a closed channel".to_string(),
            ))
            .unwrap();
        app.drain_model_events();

        assert!(
            app.toast.is_none(),
            "user-initiated stop must not toast an error"
        );
        assert_eq!(app.status_line, "turn interrupted");
        assert!(app.cancel_requested_at.is_none());
        assert!(matches!(
            app.transcript.last(),
            Some(TranscriptItem::Message(message))
                if message.content == TURN_INTERRUPTED_NOTE
        ));
    }

    /// [8]: Esc raced a natural finish — the worker's Done is already in the
    /// channel when the user asks to stop. The stop intent must win: the turn
    /// finalizes as an interruption, never as a clean completion.
    #[test]
    fn done_racing_esc_stops_the_turn_instead_of_finalizing_normally() {
        let (mut app, sender, token) = working_app();
        token.cancel();
        app.cancel_requested_at = Some(Instant::now());

        sender
            .send(ModelStreamEvent::Done { event_count: 3 })
            .unwrap();
        app.drain_model_events();

        assert!(app.turn_cancel.is_none());
        assert!(app.cancel_requested_at.is_none());
        assert!(!app.is_working());
        assert_eq!(app.status_line, "turn interrupted");
        assert!(
            app.transcript.iter().any(|item| matches!(
                item,
                TranscriptItem::Message(message) if message.content == TURN_INTERRUPTED_NOTE
            )),
            "a stopped turn must leave the interruption note"
        );
    }

    /// [8] + [21]: Esc racing the natural Done through the real key path must
    /// stop the turn and NEVER launch a queued follow-up; the queued prompts
    /// are kept for the user.
    #[test]
    fn esc_racing_done_stops_and_keeps_queued_turns() {
        let (mut app, sender, _token) = working_app();
        app.queued_turns.push_back("queued task one".to_string());
        app.queued_turns.push_back("queued task two".to_string());

        // Worker already finished: Done is sitting in the channel, but the
        // UI has not drained it yet, so is_working() is still true.
        sender
            .send(ModelStreamEvent::Done { event_count: 3 })
            .unwrap();
        assert!(app.is_working());

        // User presses Esc to stop everything (real key path).
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.status_line, "cancelling… esc again to force-stop");
        assert!(app.cancel_requested_at.is_some());

        // Next frame drains the channel and hits the Done event.
        app.drain_model_events();

        // [8]: the cancel intent wins — the turn stops.
        assert!(app.cancel_requested_at.is_none());
        assert_eq!(app.status_line, "turn interrupted");
        // [8]: no queued turn was launched — its user message never entered
        // the transcript.
        assert!(
            !app.transcript.iter().any(|item| matches!(
                item,
                TranscriptItem::Message(message)
                    if message.role == ChatRole::User && message.content == "queued task one"
            )),
            "a cancel intent must never silently launch the next queued turn"
        );
        // [21]: both queued prompts are kept, in order, not discarded.
        assert_eq!(
            app.queued_turns
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec!["queued task one", "queued task two"],
        );
    }

    /// [5]: Esc while a background workflow runs (no model turn streaming) must
    /// cancel the workflow, never fall through to the double-esc quit arm that
    /// would kill subagents mid file_edit/file_patch.
    #[test]
    fn escape_cancels_background_workflow_instead_of_quitting() {
        let mut app = app();
        let (_sender, receiver) = mpsc::channel::<WorkflowEvent>();
        let workflow = background_workflow(&app, receiver);
        let token = workflow.cancel.clone();
        app.workflow_events.push(workflow);
        let view = WorkflowRunView {
            id: "workflow-test".to_string(),
            title: "refactor auth".to_string(),
            task: "refactor auth".to_string(),
            status: WorkflowViewState::Running,
            phases: Vec::new(),
            summary: String::new(),
            expanded: false,
        };
        app.workflows.push(view.clone());
        app.transcript.push(TranscriptItem::Workflow(view));

        // A background workflow makes is_working() false but keeps work active.
        assert!(!app.is_working());
        assert!(app.has_active_workflows());

        // First Esc: cancel the workflow, mark its row, never arm quit.
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!app.should_quit);
        assert!(
            token.is_cancelled(),
            "workflow cancel token must be flipped"
        );
        assert!(
            app.last_escape_at.is_none(),
            "cancelling a workflow must not arm the double-esc quit"
        );
        assert_eq!(app.workflows[0].status, WorkflowViewState::Failed);

        // Second Esc while the worker is still attached must still not quit.
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(
            !app.should_quit,
            "double-esc must never quit while a workflow is active"
        );
    }

    /// [21]: cancelling a turn must keep the queued follow-up prompts (the UI
    /// acknowledged each) and tell the user — never silently discard them.
    #[test]
    fn cancelling_a_turn_keeps_queued_prompts_and_tells_the_user() {
        let (mut app, _sender, _token) = working_app();
        app.queued_turns
            .push_back("also update the changelog".to_string());

        app.finalize_cancelled_turn("turn interrupted");

        assert_eq!(
            app.queued_turns
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec!["also update the changelog"],
        );
        assert!(
            app.toast
                .as_ref()
                .is_some_and(|toast| toast.message.contains("kept")),
            "user must be told the queued prompt was kept: {:?}",
            app.toast.as_ref().map(|toast| &toast.message)
        );
    }

    /// [21]: kept queued prompts run on an explicit empty submit while idle,
    /// never a silent auto-launch.
    #[test]
    fn empty_submit_runs_kept_queued_prompt_when_idle() {
        let mut app = app();
        app.queued_turns
            .push_back("run the kept prompt".to_string());

        app.submit_input(); // empty composer

        assert!(
            app.queued_turns.is_empty(),
            "empty submit must run the kept prompt"
        );
        assert!(
            app.transcript.iter().any(|item| matches!(
                item,
                TranscriptItem::Message(ChatMessage { role: ChatRole::User, content, .. })
                    if content == "run the kept prompt"
            )),
            "the kept prompt must be started"
        );
    }

    #[test]
    fn usage_events_accumulate_across_requests_within_a_turn() {
        let (mut app, sender, _token) = working_app();

        // A tool-looping turn makes one request per iteration; each reports
        // its own usage and the TUI must count all of them.
        sender
            .send(ModelStreamEvent::Usage(TokenUsage {
                input: 100,
                output: 10,
                cached: 40,
            }))
            .unwrap();
        sender
            .send(ModelStreamEvent::Usage(TokenUsage {
                input: 250,
                output: 30,
                cached: 200,
            }))
            .unwrap();
        sender
            .send(ModelStreamEvent::Done { event_count: 5 })
            .unwrap();
        app.drain_model_events();

        let expected = TokenUsage {
            input: 350,
            output: 40,
            cached: 240,
        };
        assert_eq!(app.session_usage, expected);
        assert_eq!(app.session_requests, 2);
        assert_eq!(app.last_turn_usage, expected);
        assert_eq!(app.last_turn_requests, 2);
    }

    #[test]
    fn cancelled_turn_still_freezes_last_turn_usage() {
        let (mut app, sender, _token) = working_app();

        sender
            .send(ModelStreamEvent::Usage(TokenUsage {
                input: 10,
                output: 2,
                cached: 0,
            }))
            .unwrap();
        sender.send(ModelStreamEvent::Cancelled).unwrap();
        app.drain_model_events();

        assert_eq!(app.last_turn_requests, 1);
        assert_eq!(app.last_turn_usage.input, 10);
        assert_eq!(app.session_requests, 1);
    }

    #[test]
    fn token_counts_format_compactly() {
        assert_eq!(format_token_count(0), "0 tok");
        assert_eq!(format_token_count(812), "812 tok");
        assert_eq!(format_token_count(1_234), "1.23k tok");
        assert_eq!(format_token_count(60_000), "60.00k tok");
        assert_eq!(format_token_count(2_050_000), "2.05M tok");
    }

    #[test]
    fn context_report_categories_sum_to_total_estimate() {
        let mut app = app();
        app.transcript = vec![
            TranscriptItem::Message(ChatMessage::user("write the missing tests")),
            TranscriptItem::Reasoning(ReasoningTrace {
                content: "inspecting the test module first".to_string(),
                expanded: false,
            }),
            TranscriptItem::Plan(PlanView {
                summary: "test plan".to_string(),
                items: vec![PlanItemView {
                    text: "add coverage".to_string(),
                    status: PlanItemStatus::Pending,
                    evidence: Vec::new(),
                }],
                expanded: false,
            }),
        ];
        app.push_tool_start("file_read".to_string(), "src/main.rs".to_string());

        let chars = transcript_char_usage(&app.transcript);
        assert_eq!(chars.total(), app.context_usage_chars());
        assert!(chars.messages > 0);
        assert!(chars.tool_outputs > 0);
        assert!(chars.reasoning > 0);
        assert!(chars.plans > 0);

        let report = app.build_context_report();
        // Transcript categories reuse the same ~4 chars/token estimate.
        assert_eq!(report.message_tokens, chars.messages.div_ceil(4));
        assert_eq!(report.tool_tokens, chars.tool_outputs.div_ceil(4));
        assert_eq!(report.reasoning_tokens, chars.reasoning.div_ceil(4));
        assert_eq!(report.plan_tokens, chars.plans.div_ceil(4));
        // The report's total is exactly the sum of its categories.
        assert_eq!(
            report.total_tokens(),
            report.instructions_tokens
                + report.system_tokens
                + report.message_tokens
                + report.tool_tokens
                + report.reasoning_tokens
                + report.plan_tokens
        );
        assert!(
            report.instructions_tokens > 0,
            "system prompt estimate missing"
        );
        assert!(report.system_tokens > 0, "session header estimate missing");
        assert!(report.budget >= 1_000);
        assert!(report.summary_covers.is_none());
        assert_eq!(report.summary_tokens, 0);
    }

    #[test]
    fn cost_and_context_commands_open_modals() {
        let mut app = app();

        assert!(app.run_local_tool_command("/cost"));
        assert_eq!(app.active_modal, Some(Modal::Cost));

        app.active_modal = None;
        assert!(app.run_local_tool_command("/context"));
        assert_eq!(app.active_modal, Some(Modal::Context));
        assert!(app.context_report.is_some());
    }

    #[test]
    fn compact_refuses_while_a_turn_is_streaming() {
        let (mut app, _sender, _token) = working_app();

        assert!(app.run_local_tool_command("/compact"));

        assert!(app.compact_events.is_none(), "no compact worker may start");
        assert!(matches!(
            app.toast,
            Some(Toast {
                kind: ToastKind::Warning,
                ..
            })
        ));
    }

    #[test]
    fn compact_result_lands_as_success_toast_with_before_and_after() {
        let mut app = app();
        let (sender, receiver) = mpsc::channel();
        app.compact_events = Some(receiver);
        sender
            .send(Ok(ManualCompaction {
                before_tokens: 12_000,
                after_tokens: 3_000,
                folded_messages: 14,
            }))
            .unwrap();

        assert!(app.drain_compact_events());

        assert!(app.compact_events.is_none());
        let toast = app.toast.clone().expect("compact toast");
        assert_eq!(toast.kind, ToastKind::Success);
        assert!(toast.message.contains("12.00k tok"), "{}", toast.message);
        assert!(toast.message.contains("3.00k tok"), "{}", toast.message);
        assert!(
            toast.message.contains("14 messages folded"),
            "{}",
            toast.message
        );
    }

    #[test]
    fn compact_failure_lands_as_error_toast() {
        let mut app = app();
        let (sender, receiver) = mpsc::channel();
        app.compact_events = Some(receiver);
        sender.send(Err("backend offline".to_string())).unwrap();

        assert!(app.drain_compact_events());

        assert!(app.compact_events.is_none());
        let toast = app.toast.clone().expect("compact toast");
        assert_eq!(toast.kind, ToastKind::Error);
        assert!(
            toast.message.contains("backend offline"),
            "{}",
            toast.message
        );
    }

    #[test]
    fn always_allow_settles_queued_siblings_and_persists() {
        let (mut app, workspace) = app_in_workspace();
        let first = queue_approval(&mut app, "cargo test -p medusa-core");
        let sibling = queue_approval(&mut app, "cargo test -p medusa-tui");

        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));

        assert_eq!(first.try_recv().unwrap(), ApprovalDecision::AlwaysAllow);
        // The sibling with the same derived prefix resolved without a prompt.
        assert_eq!(sibling.try_recv().unwrap(), ApprovalDecision::AllowOnce);
        assert!(app.approval_queue.is_empty());

        let persisted = fs::read_to_string(workspace.join(".medusa/permissions.json")).unwrap();
        assert!(persisted.contains("cargo test"));
    }

    #[test]
    fn approval_keys_do_not_leak_into_composer() {
        let mut app = app();
        let _decision = queue_approval(&mut app, "cargo build");

        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(app.input.is_empty(), "keys must not reach the composer");
        assert_eq!(app.approval_queue.len(), 1);

        app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        assert!(app.input.is_empty());
        assert!(app.approval_queue.is_empty());
    }

    #[test]
    fn approval_keys_are_ignored_during_grace_window() {
        let mut app = app();
        let (respond, decision) = mpsc::channel();
        app.approval_queue.push_back(PendingApproval {
            request: ApprovalRequest {
                tool: ApprovalTool::TerminalExec,
                command: Some("cargo build".to_string()),
                paths: Vec::new(),
                background: false,
                sandbox_escalation: false,
            },
            respond,
        });
        app.approval_shown_at = Some(Instant::now());

        app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        assert!(decision.try_recv().is_err());
        assert_eq!(app.approval_queue.len(), 1);

        app.approval_shown_at = Instant::now().checked_sub(APPROVAL_KEY_GRACE * 2);
        app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        assert_eq!(decision.try_recv().unwrap(), ApprovalDecision::AllowOnce);
    }

    #[test]
    fn ctrl_modified_keys_never_decide_an_approval() {
        let mut app = app();
        let decision = queue_approval(&mut app, "cargo build");

        // Ctrl+A (readline home) must not trigger AlwaysAllow.
        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert!(decision.try_recv().is_err());
        assert_eq!(app.approval_queue.len(), 1);

        // Plain 'a' still works.
        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        assert_eq!(decision.try_recv().unwrap(), ApprovalDecision::AlwaysAllow);
    }

    #[test]
    fn sandbox_escalations_never_settle_against_stored_grants() {
        let mut app = app();
        app.session_terminal_grants.push("cargo build".to_string());

        // The very same command settles from the grant when sandboxed...
        assert_eq!(
            app.auto_approval_decision(&ApprovalRequest {
                tool: ApprovalTool::TerminalExec,
                command: Some("cargo build".to_string()),
                paths: Vec::new(),
                background: false,
                sandbox_escalation: false,
            }),
            Some(ApprovalDecision::AllowOnce)
        );
        // ...but an escalation of it must always reach a human.
        assert_eq!(
            app.auto_approval_decision(&ApprovalRequest {
                tool: ApprovalTool::TerminalExec,
                command: Some("cargo build".to_string()),
                paths: Vec::new(),
                background: false,
                sandbox_escalation: true,
            }),
            None
        );
    }

    #[test]
    fn always_allow_key_cannot_decide_a_sandbox_escalation() {
        let mut app = app();
        let decision = queue_approval_kind(&mut app, "cargo build", true);

        // 'a' is not offered on escalation cards and must be inert.
        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        assert!(decision.try_recv().is_err());
        assert_eq!(app.approval_queue.len(), 1);
        assert!(app.session_terminal_grants.is_empty());

        // Allow-once still resolves it without recording any grant.
        app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        assert_eq!(decision.try_recv().unwrap(), ApprovalDecision::AllowOnce);
        assert!(app.session_terminal_grants.is_empty());
    }

    #[test]
    fn escalation_always_allow_decision_downgrades_to_allow_once() {
        let mut app = app();
        let decision = queue_approval_kind(&mut app, "cargo build", true);

        // Even if an AlwaysAllow decision reaches an escalation through some
        // other path, nothing may be persisted.
        app.resolve_pending_approval(ApprovalDecision::AlwaysAllow);

        assert_eq!(decision.try_recv().unwrap(), ApprovalDecision::AllowOnce);
        assert!(app.session_terminal_grants.is_empty());
    }

    #[test]
    fn env_prefixed_commands_settle_against_grants() {
        let mut app = app();
        app.session_terminal_grants.push("cargo build".to_string());

        assert_eq!(
            app.auto_approval_decision(&ApprovalRequest {
                tool: ApprovalTool::TerminalExec,
                command: Some("FOO=bar cargo build --release".to_string()),
                paths: Vec::new(),
                background: false,
                sandbox_escalation: false,
            }),
            Some(ApprovalDecision::AllowOnce)
        );
    }

    #[test]
    fn edit_grants_do_not_leak_to_prefix_siblings() {
        let mut app = app();
        app.session_edit_grants.push("Cargo.toml".to_string());
        app.session_edit_grants.push("src/".to_string());

        let granted = |app: &App, p: &str| {
            app.auto_approval_decision(&ApprovalRequest {
                tool: ApprovalTool::FileEdit,
                command: None,
                paths: vec![p.to_string()],
                background: false,
                sandbox_escalation: false,
            }) == Some(ApprovalDecision::AllowOnce)
        };

        assert!(granted(&app, "Cargo.toml"));
        assert!(!granted(&app, "Cargo.toml.bak")); // exact-match, no leak
        assert!(granted(&app, "src/main.rs")); // dir subtree
        assert!(!granted(&app, "src-evil/x.rs"));
    }

    #[test]
    fn denied_edits_are_remembered_for_the_turn() {
        let mut app = app();
        let (respond, _decision) = mpsc::channel();
        app.approval_queue.push_back(PendingApproval {
            request: ApprovalRequest {
                tool: ApprovalTool::FileEdit,
                command: None,
                paths: vec!["src/secret.rs".to_string()],
                background: false,
                sandbox_escalation: false,
            },
            respond,
        });
        app.approval_shown_at = Instant::now().checked_sub(APPROVAL_KEY_GRACE * 2);
        app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));

        // A retry of the same edit auto-denies instead of re-prompting.
        assert_eq!(
            app.auto_approval_decision(&ApprovalRequest {
                tool: ApprovalTool::FileEdit,
                command: None,
                paths: vec!["src/secret.rs".to_string()],
                background: false,
                sandbox_escalation: false,
            }),
            Some(ApprovalDecision::Deny)
        );
    }

    #[test]
    fn long_commands_wrap_instead_of_hiding_their_tail() {
        let long = format!("echo {} && rm -rf important", "x".repeat(200));
        let lines = wrap_str(&long, 40);
        assert!(lines.len() > 1);
        // The destructive tail is present somewhere in the wrapped output.
        assert!(lines.iter().any(|line| line.contains("rm -rf important")));
    }

    #[test]
    fn interpreter_grants_are_never_persisted() {
        for command in [
            "bash scripts/lint.sh",
            "python gen.py",
            "sudo rm x",
            "env FOO=1 python evil.py",
            "xargs rm",
        ] {
            assert_eq!(
                derive_terminal_grant_prefix(command),
                None,
                "`{command}` must not yield a persistable grant prefix"
            );
        }
    }

    #[test]
    fn terminal_grant_prefixes_are_derived_conservatively() {
        assert_eq!(
            derive_terminal_grant_prefix("cargo test -p medusa-core"),
            Some("cargo test".to_string())
        );
        assert_eq!(
            derive_terminal_grant_prefix("npm run build --watch"),
            Some("npm run build".to_string())
        );
        assert_eq!(
            derive_terminal_grant_prefix("rustfmt src/main.rs"),
            Some("rustfmt".to_string())
        );
        assert_eq!(
            derive_terminal_grant_prefix("FOO=bar cargo build"),
            Some("cargo build".to_string())
        );
        assert_eq!(derive_terminal_grant_prefix("cargo test && rm -rf /"), None);
        assert_eq!(derive_terminal_grant_prefix("echo hi | sh"), None);
        assert_eq!(derive_terminal_grant_prefix(""), None);
    }

    #[test]
    fn removed_viewer_commands_report_unknown() {
        let mut app = app();

        for command in ["/images", "/themes", "/demo", "/recap"] {
            assert!(app.run_local_tool_command(command));
        }
        assert!(matches!(
            app.transcript.last(),
            Some(TranscriptItem::Message(ChatMessage { content, .. }))
                if content.contains("unknown command: /recap")
        ));
    }

    #[test]
    fn plan_updates_render_in_strip_not_transcript() {
        let mut app = app();
        app.apply_plan_update_output(
            r#"{"summary":"Ship plan UI","items":[{"text":"Inspect current renderer","status":"done","evidence":["main.rs"]},{"text":"Render plan rows","status":"active"},{"text":"Run tests","status":"pending"}]}"#,
        )
        .unwrap();

        let Some(plan) = app.current_plan() else {
            panic!("expected current plan");
        };
        assert_eq!(plan.summary, "Ship plan UI");
        assert_eq!(plan.items[1].status, PlanItemStatus::Active);

        // Not in the chat transcript…
        let lines = visible_transcript_lines(&app.transcript, None, None);
        let text = lines.iter().map(line_text).collect::<Vec<_>>();
        assert!(!text.iter().any(|line| line.contains("Render plan rows")));

        // …but in the strip above the composer.
        let strip = plan_strip_lines(app.plan_strip().expect("strip visible"))
            .iter()
            .map(line_text)
            .collect::<Vec<_>>();
        assert!(strip[0].contains("plan · 1/3"));
        assert!(strip[0].contains("Ship plan UI"));
        assert!(strip.iter().any(|line| line.contains("Render plan rows")));
        assert!(app.plan_strip_height(40) > 0);
    }

    #[test]
    fn completed_plan_leaves_the_strip() {
        let mut app = app();
        app.apply_plan_update_output(
            r#"{"summary":"Done","items":[{"text":"one","status":"done"},{"text":"two","status":"done"}]}"#,
        )
        .unwrap();

        assert!(app.plan_strip().is_none());
        assert_eq!(app.plan_strip_height(40), 0);
    }

    #[test]
    fn long_plan_strip_folds_completed_prefix_and_tail() {
        let mut app = app();
        let items = (1..=12)
            .map(|index| {
                let status = if index <= 4 {
                    "done"
                } else if index == 5 {
                    "active"
                } else {
                    "pending"
                };
                format!(r#"{{"text":"step {index}","status":"{status}"}}"#)
            })
            .collect::<Vec<_>>()
            .join(",");
        app.apply_plan_update_output(&format!(r#"{{"summary":"Big","items":[{items}]}}"#))
            .unwrap();

        let strip = plan_strip_lines(app.plan_strip().expect("strip visible"))
            .iter()
            .map(line_text)
            .collect::<Vec<_>>();
        assert!(strip.iter().any(|line| line.contains("✓ 4 done")));
        assert!(strip.iter().any(|line| line.contains("step 5")));
        assert!(!strip.iter().any(|line| line.contains("step 1 ")));
        assert!(
            strip.last().unwrap().contains("… "),
            "tail folds: {strip:?}"
        );
    }

    #[test]
    fn consecutive_plan_updates_replace_latest_snapshot() {
        let mut app = app();

        app.apply_plan_update_output(
            r#"{"summary":"First","items":[{"text":"one","status":"active"}]}"#,
        )
        .unwrap();
        app.apply_plan_update_output(
            r#"{"summary":"Second","items":[{"text":"one","status":"done"},{"text":"two","status":"active","evidence":["cargo check"]}]}"#,
        )
        .unwrap();

        assert_eq!(app.transcript.len(), 1);
        let Some(plan) = app.current_plan() else {
            panic!("expected current plan");
        };
        assert_eq!(plan.summary, "Second");
        assert_eq!(plan.items[0].status, PlanItemStatus::Done);
        assert!(
            plan.items[1]
                .evidence
                .iter()
                .any(|line| line.contains("cargo check"))
        );
    }

    #[test]
    fn decision_request_output_renders_inline() {
        let mut app = app();

        app.apply_decision_request_output(
            r#"{"title":"Choose storage","reason":"Storage changes how the plan is implemented.","questions":[{"id":"storage","prompt":"Where should plans live?","kind":"choice","options":["transcript","file"],"recommended":"transcript","required":true}],"assumptions":["Use transcript if the user does not care."]}"#,
        )
        .unwrap();

        let Some(decision) = app.pending_decision() else {
            panic!("expected pending decision");
        };
        assert_eq!(decision.title, "Choose storage");
        assert_eq!(decision.questions.len(), 1);
        assert_eq!(decision.questions[0].kind, DecisionQuestionKind::Choice);

        let lines = visible_transcript_lines(&app.transcript, None, None);
        let text = lines.iter().map(line_text).collect::<Vec<_>>();
        assert!(text.iter().any(|line| line.contains("decision")));
        assert!(text.iter().any(|line| line.contains("waiting")));
        assert!(
            text.iter()
                .any(|line| line.contains("Where should plans live?"))
        );
        assert!(text.iter().any(|line| line.contains("Choose storage")));
        assert!(text.iter().any(|line| line.contains("transcript")));
    }

    #[test]
    fn answering_pending_decision_marks_it_answered() {
        let mut app = app();
        app.apply_decision_request_output(
            r#"{"title":"Pick approach","questions":[{"id":"approach","prompt":"Which path?","kind":"text","options":[],"required":true}]}"#,
        )
        .unwrap();

        app.input = "Use the simple transcript path.".to_string();
        app.input_cursor = app.input_len();
        app.submit_input();

        let Some(decision) = app.current_decision() else {
            panic!("expected decision");
        };
        assert!(decision.answered);
        assert_eq!(
            decision.answers.get("approach").map(String::as_str),
            Some("Use the simple transcript path.")
        );
        assert!(
            decision.answer.as_deref().is_some_and(
                |answer| answer.contains("- approach: Use the simple transcript path.")
            )
        );
        assert_eq!(
            app.toast.as_ref().map(|toast| toast.kind),
            Some(ToastKind::Success)
        );
    }

    #[test]
    fn decision_choice_can_be_selected_with_keyboard_and_submitted() {
        let mut app = app();
        app.apply_decision_request_output(
            r#"{"title":"Semantic indexing","questions":[{"id":"embedding","prompt":"Which embedding approach?","kind":"choice","options":["local embeddings","remote API embeddings","hybrid"],"recommended":"local embeddings","required":true},{"id":"timing","prompt":"When should it run?","kind":"choice","options":["manual only","lazy on first search","background on startup"],"recommended":"lazy on first search","required":true}]}"#,
        )
        .unwrap();

        app.handle_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE));
        assert_eq!(
            app.pending_decision()
                .and_then(|decision| decision.answers.get("embedding"))
                .map(String::as_str),
            Some("remote API embeddings")
        );

        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        let Some(decision) = app.current_decision() else {
            panic!("expected decision");
        };
        assert!(decision.answered);
        assert_eq!(
            decision.answers.get("timing").map(String::as_str),
            Some("background on startup")
        );
        assert!(matches!(
            app.transcript.last(),
            Some(TranscriptItem::Message(ChatMessage { role: ChatRole::User, content, .. }))
                if content.contains("Decision answer: Semantic indexing")
                    && content.contains("- embedding: remote API embeddings")
                    && content.contains("- timing: background on startup")
        ));
    }

    #[test]
    fn text_decision_questions_do_not_steal_regular_typing_keys() {
        let mut app = app();
        app.apply_decision_request_output(
            r#"{"title":"Name workflow","questions":[{"id":"name","prompt":"What should it be called?","kind":"text","options":[],"required":true}]}"#,
        )
        .unwrap();

        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE));

        assert_eq!(app.input, "jit");
        assert_eq!(app.decision_selection, 0);
        assert_eq!(app.selected_tool, None);
    }

    #[test]
    fn tool_output_failure_detects_nonzero_exit() {
        assert!(!tool_output_failed("exit: 0\nstdout:\nok"));
        assert!(tool_output_failed("exit: 101\nstderr:\nfailed"));
        assert!(tool_output_failed("error: nope"));
    }

    #[test]
    fn visible_tool_activity_lines_show_running_state() {
        let transcript = vec![TranscriptItem::Tool(ToolRun {
            id: None,
            started_at: Instant::now(),
            pending_result: None,
            name: "file.patch".to_string(),
            summary: "apply patch".to_string(),
            state: ToolRunState::Running,
            detail: String::new(),
            expanded: false,
            group_expanded: false,
        })];

        let lines = visible_transcript_lines(&transcript, None, None);
        let text = lines.iter().map(line_text).collect::<Vec<_>>();

        assert!(text[0].contains("patch apply patch"));
        assert!(
            text[0].contains("⠁")
                || text[0].contains("⠃")
                || text[0].contains("⠇")
                || text[0].contains("⠧")
                || text[0].contains("⠷")
                || text[0].contains("⠿")
        );
        assert!(text[1].contains("⎿ running…"));
    }

    #[test]
    fn running_tool_pulse_animation_changes_braille_only() {
        let run = ToolRun {
            id: None,
            started_at: Instant::now(),
            pending_result: None,
            name: "terminal.exec".to_string(),
            summary: "$ cargo test".to_string(),
            state: ToolRunState::Running,
            detail: String::new(),
            expanded: false,
            group_expanded: false,
        };

        let mut first = Vec::new();
        append_tool_call_lines(
            &mut first,
            &run,
            false,
            RenderContext {
                animation_tick: 0,
                ..Default::default()
            },
        );
        let mut second = Vec::new();
        append_tool_call_lines(
            &mut second,
            &run,
            false,
            RenderContext {
                animation_tick: 12,
                ..Default::default()
            },
        );

        let first_text = first.iter().map(line_text).collect::<Vec<_>>();
        let second_text = second.iter().map(line_text).collect::<Vec<_>>();

        assert_ne!(first_text[0], second_text[0]);
        assert_eq!(first_text[1], second_text[1]);
        assert!(!second_text[0].contains("⬤"));
        assert!(second_text[0].contains("⠧"));
        assert!(second_text[1].contains("running"));
    }

    #[test]
    fn tool_calls_render_one_block_per_call() {
        let read = ToolRun {
            id: None,
            started_at: Instant::now(),
            pending_result: None,
            name: "file.read".to_string(),
            summary: "crates/medusa-tui/src/main.rs".to_string(),
            state: ToolRunState::Succeeded,
            detail: "read 1 • crates/medusa-tui/src/main.rs".to_string(),
            expanded: false,
            group_expanded: false,
        };
        let failed_patch = ToolRun {
            id: None,
            started_at: Instant::now(),
            pending_result: None,
            name: "file.patch".to_string(),
            summary: "apply patch".to_string(),
            state: ToolRunState::Failed,
            detail: "error: patch rejected\ncontext mismatch at line 4".to_string(),
            expanded: false,
            group_expanded: false,
        };
        let transcript = vec![
            TranscriptItem::Tool(read),
            TranscriptItem::Tool(failed_patch),
        ];

        let mut lines = Vec::new();
        append_tool_group_lines(
            &mut lines,
            &transcript,
            0,
            transcript.len(),
            None,
            RenderContext::static_view(),
        );
        let text = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("• read crates/medusa-tui/src/main.rs"));
        assert!(text.contains("⎿ read 1 • crates/medusa-tui/src/main.rs"));
        assert!(text.contains("• patch apply patch"));
        assert!(text.contains("⎿ error: patch rejected"));
        assert!(text.contains("context mismatch at line 4"));
    }

    #[test]
    fn collapsed_tool_output_shows_expand_hint() {
        let noisy = ToolRun {
            id: None,
            started_at: Instant::now(),
            pending_result: None,
            name: "terminal.exec".to_string(),
            summary: "$ cargo test".to_string(),
            state: ToolRunState::Succeeded,
            detail: (1..=6)
                .map(|index| format!("output line {index}"))
                .collect::<Vec<_>>()
                .join("\n"),
            expanded: false,
            group_expanded: false,
        };

        let mut lines = Vec::new();
        append_tool_call_lines(&mut lines, &noisy, false, RenderContext::static_view());
        let text = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("⎿ output line 1"));
        assert!(!text.contains("output line 2"));
        assert!(text.contains("+5 lines (enter to expand)"));

        let mut expanded_run = noisy;
        expanded_run.expanded = true;
        let mut expanded_lines = Vec::new();
        append_tool_call_lines(
            &mut expanded_lines,
            &expanded_run,
            false,
            RenderContext::static_view(),
        );
        let expanded_text = expanded_lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(expanded_text.contains("output line 6"));
        assert!(!expanded_text.contains("enter to expand"));
    }

    #[test]
    fn consecutive_tool_rows_render_as_one_activity_block() {
        let transcript = vec![
            TranscriptItem::Tool(ToolRun {
                id: None,
                started_at: Instant::now(),
                pending_result: None,
                name: "terminal.exec".to_string(),
                summary: "$ cargo test -p medusa-tui".to_string(),
                state: ToolRunState::Succeeded,
                detail: "24 passed".to_string(),
                expanded: false,
                group_expanded: false,
            }),
            TranscriptItem::Tool(ToolRun {
                id: None,
                started_at: Instant::now(),
                pending_result: None,
                name: "file.patch".to_string(),
                summary: "crates/medusa-tui/src/main.rs - update renderer".to_string(),
                state: ToolRunState::Failed,
                detail: "error: patch rejected\nrecovery: inspect context".to_string(),
                expanded: false,
                group_expanded: false,
            }),
        ];

        let lines = visible_transcript_lines(&transcript, None, None);
        let text = lines.iter().map(line_text).collect::<Vec<_>>();

        assert!(
            text.iter()
                .any(|line| line.contains("terminal $ cargo test -p medusa-tui"))
        );
        assert!(text.iter().any(|line| line.contains("⎿ 24 passed")));
        assert!(
            text.iter()
                .any(|line| line.contains("patch crates/medusa-tui/src/main.rs - update renderer"))
        );
        assert!(
            text.iter()
                .any(|line| line.contains("⎿ error: patch rejected"))
        );
        assert!(
            text.iter()
                .any(|line| line.contains("recovery: inspect context"))
        );
    }

    fn finished_tool(name: &str, summary: &str, state: ToolRunState) -> ToolRun {
        ToolRun {
            id: None,
            started_at: Instant::now(),
            pending_result: None,
            name: name.to_string(),
            summary: summary.to_string(),
            state,
            detail: "done".to_string(),
            expanded: false,
            group_expanded: false,
        }
    }

    #[test]
    fn consecutive_same_tool_calls_coalesce_into_one_line() {
        let transcript = vec![
            TranscriptItem::Tool(finished_tool(
                "file.read",
                "read src/main.rs",
                ToolRunState::Succeeded,
            )),
            TranscriptItem::Tool(finished_tool(
                "file.read",
                "read src/tools.rs",
                ToolRunState::Succeeded,
            )),
            TranscriptItem::Tool(finished_tool(
                "file.read",
                "read src/wire.rs",
                ToolRunState::Succeeded,
            )),
            TranscriptItem::Tool(finished_tool(
                "file.read",
                "read src/exec.rs",
                ToolRunState::Succeeded,
            )),
            TranscriptItem::Tool(finished_tool(
                "terminal.exec",
                "$ cargo check",
                ToolRunState::Succeeded,
            )),
        ];

        let lines = visible_transcript_lines(&transcript, None, None);
        let text = lines.iter().map(line_text).collect::<Vec<_>>();

        assert!(
            text.iter().any(|line| {
                line.contains("read src/main.rs, src/tools.rs, src/wire.rs +1 more")
            })
        );
        assert!(text.iter().any(|line| line.contains("⎿ 4 calls")));
        // The lone terminal call renders as a normal block.
        assert!(
            text.iter()
                .any(|line| line.contains("terminal $ cargo check"))
        );
        assert!(!text.iter().any(|line| line.contains("read src/tools.rs\n")));
    }

    #[test]
    fn out_of_order_tool_results_land_on_the_right_blocks_by_call_id() {
        let mut app = app();
        app.push_tool_start_with_id(
            Some("call_a".to_string()),
            "file.read".to_string(),
            "read src/a.rs".to_string(),
        );
        app.push_tool_start_with_id(
            Some("call_b".to_string()),
            "file.read".to_string(),
            "read src/b.rs".to_string(),
        );
        for item in &mut app.transcript {
            if let TranscriptItem::Tool(run) = item {
                run.started_at = run
                    .started_at
                    .checked_sub(MIN_TOOL_PULSE_VISIBLE)
                    .unwrap_or(run.started_at);
            }
        }

        // Second call finishes first (parallel execution), then the first.
        app.push_tool_result_for_call("call_b", "file.read", "content of b".to_string());
        app.push_tool_result_for_call("call_a", "file.read", "content of a".to_string());

        let TranscriptItem::Tool(first) = &app.transcript[0] else {
            panic!("expected tool run");
        };
        let TranscriptItem::Tool(second) = &app.transcript[1] else {
            panic!("expected tool run");
        };
        assert_eq!(first.detail, "content of a");
        assert_eq!(second.detail, "content of b");
        assert_eq!(first.state, ToolRunState::Succeeded);
        assert_eq!(second.state, ToolRunState::Succeeded);
    }

    #[test]
    fn edit_tool_shows_diff_lines_and_never_coalesces() {
        let mut first = finished_tool("file.edit", "edit src/a.rs", ToolRunState::Succeeded);
        first.detail =
            "edited src/a.rs (1 replacement)\n- fn old() {}\n+ fn new() {}\n  shared".to_string();
        let mut second = finished_tool("file.edit", "edit src/b.rs", ToolRunState::Succeeded);
        second.detail = "edited src/b.rs (1 replacement)\n- x\n+ y".to_string();
        let transcript = vec![TranscriptItem::Tool(first), TranscriptItem::Tool(second)];

        let lines = visible_transcript_lines(&transcript, None, None);
        let text = lines.iter().map(line_text).collect::<Vec<_>>();

        // Both edits stay as separate blocks with their diff bodies visible.
        assert!(text.iter().any(|line| line.contains("edit src/a.rs")));
        assert!(text.iter().any(|line| line.contains("edit src/b.rs")));
        assert!(text.iter().any(|line| line.contains("- fn old() {}")));
        assert!(text.iter().any(|line| line.contains("+ fn new() {}")));
        assert!(!text.iter().any(|line| line.contains("2 calls")));
    }

    #[test]
    fn running_call_joins_coalesced_run_as_live_tail() {
        let mut running = finished_tool("file.read", "read src/slow.rs", ToolRunState::Running);
        running.detail = String::new();
        let transcript = vec![
            TranscriptItem::Tool(finished_tool(
                "file.read",
                "read src/main.rs",
                ToolRunState::Succeeded,
            )),
            TranscriptItem::Tool(finished_tool(
                "file.read",
                "read src/tools.rs",
                ToolRunState::Succeeded,
            )),
            TranscriptItem::Tool(running),
        ];

        let lines = visible_transcript_lines(&transcript, None, None);
        let text = lines.iter().map(line_text).collect::<Vec<_>>();

        assert!(
            text.iter()
                .any(|line| line.contains("read src/main.rs, src/tools.rs, src/slow.rs")),
            "running call renders inside the coalesced line, not below it"
        );
        assert!(
            text.iter()
                .any(|line| line.contains("⎿ 3 calls · running…"))
        );
        assert_eq!(
            text.iter()
                .filter(|line| line.contains("src/slow.rs"))
                .count(),
            1,
            "the running call must not also render as its own block"
        );
    }

    #[test]
    fn failed_and_running_calls_never_coalesce() {
        let mut running = finished_tool("file.read", "read src/slow.rs", ToolRunState::Running);
        running.detail = String::new();
        let transcript = vec![
            TranscriptItem::Tool(finished_tool(
                "file.read",
                "read src/main.rs",
                ToolRunState::Succeeded,
            )),
            TranscriptItem::Tool(finished_tool(
                "file.read",
                "read src/missing.rs",
                ToolRunState::Failed,
            )),
            TranscriptItem::Tool(running),
        ];

        let lines = visible_transcript_lines(&transcript, None, None);
        let text = lines.iter().map(line_text).collect::<Vec<_>>();

        assert!(!text.iter().any(|line| line.contains("calls")));
        assert!(text.iter().any(|line| line.contains("read src/main.rs")));
        assert!(text.iter().any(|line| line.contains("read src/missing.rs")));
        assert!(text.iter().any(|line| line.contains("read src/slow.rs")));
    }

    #[test]
    fn reasoning_between_same_tool_calls_does_not_break_coalescing() {
        let transcript = vec![
            TranscriptItem::Tool(finished_tool(
                "file.read",
                "read src/main.rs",
                ToolRunState::Succeeded,
            )),
            TranscriptItem::Reasoning(ReasoningTrace {
                content: "Reading the next file.".to_string(),
                expanded: false,
            }),
            TranscriptItem::Tool(finished_tool(
                "file.read",
                "read src/tools.rs",
                ToolRunState::Succeeded,
            )),
        ];

        let lines = visible_transcript_lines(&transcript, None, None);
        let text = lines.iter().map(line_text).collect::<Vec<_>>();

        assert!(
            text.iter()
                .any(|line| line.contains("read src/main.rs, src/tools.rs"))
        );
        assert!(text.iter().any(|line| line.contains("⎿ 2 calls")));
    }

    #[test]
    fn enter_cycles_coalesced_group_open_then_details_then_coalesced() {
        let mut app = app();
        app.transcript.push(TranscriptItem::Tool(finished_tool(
            "file.read",
            "read src/main.rs",
            ToolRunState::Succeeded,
        )));
        app.transcript.push(TranscriptItem::Tool(finished_tool(
            "file.read",
            "read src/tools.rs",
            ToolRunState::Succeeded,
        )));
        app.transcript.push(TranscriptItem::Tool(finished_tool(
            "terminal.exec",
            "$ cargo check",
            ToolRunState::Succeeded,
        )));

        app.selected_tool = Some(0);
        app.toggle_selected_tool();
        assert!(tool_group_is_open(&app.transcript, 0, 3));
        assert!(
            !matches!(&app.transcript[0], TranscriptItem::Tool(run) if run.expanded),
            "first enter un-coalesces the group without opening details"
        );

        app.toggle_selected_tool();
        assert!(matches!(&app.transcript[0], TranscriptItem::Tool(run) if run.expanded));

        app.toggle_selected_tool();
        assert!(!tool_group_is_open(&app.transcript, 0, 3));
        assert!(!matches!(&app.transcript[0], TranscriptItem::Tool(run) if run.expanded));
    }

    #[test]
    fn interleaved_reasoning_and_tools_render_as_one_batch() {
        let transcript = vec![
            TranscriptItem::Reasoning(ReasoningTrace {
                content: "detailed Inspecting codebase.".to_string(),
                expanded: false,
            }),
            TranscriptItem::Tool(ToolRun {
                id: None,
                started_at: Instant::now(),
                pending_result: None,
                name: "terminal.exec".to_string(),
                summary: "$ rg TODO".to_string(),
                state: ToolRunState::Succeeded,
                detail: "done".to_string(),
                expanded: false,
                group_expanded: false,
            }),
            TranscriptItem::Reasoning(ReasoningTrace {
                content: "Reading matching files.".to_string(),
                expanded: false,
            }),
            TranscriptItem::Tool(ToolRun {
                id: None,
                started_at: Instant::now(),
                pending_result: None,
                name: "terminal.exec".to_string(),
                summary: "$ sed -n '1,80p' README.md".to_string(),
                state: ToolRunState::Succeeded,
                detail: "done".to_string(),
                expanded: false,
                group_expanded: false,
            }),
        ];

        let lines = visible_transcript_lines(&transcript, None, None);
        let text = lines.iter().map(line_text).collect::<Vec<_>>();

        assert!(
            text.iter()
                .any(|line| line.contains("terminal $ rg TODO, $ sed -n '1,80p' README.md"))
        );
        assert!(!text.iter().any(|line| line.contains("thinking")));
    }

    #[test]
    fn selected_tool_row_still_stays_collapsed_until_opened() {
        let transcript = vec![TranscriptItem::Tool(ToolRun {
            id: None,
            started_at: Instant::now(),
            pending_result: None,
            name: "terminal.exec".to_string(),
            summary: "$ cargo check".to_string(),
            state: ToolRunState::Succeeded,
            detail: "done".to_string(),
            expanded: false,
            group_expanded: false,
        })];

        let lines = visible_transcript_lines(&transcript, None, Some(0));

        let text = lines.iter().map(line_text).collect::<Vec<_>>();
        assert!(text[0].contains("terminal $ cargo check"));
        assert!(text[1].contains("⎿ done"));
    }

    #[test]
    fn tool_selection_enter_and_close_updates_inline_output_state() {
        let mut app = app();
        app.transcript.push(TranscriptItem::Tool(ToolRun {
            id: None,
            started_at: Instant::now(),
            pending_result: None,
            name: "terminal.exec".to_string(),
            summary: "$ cargo check".to_string(),
            state: ToolRunState::Succeeded,
            detail: "24 passed".to_string(),
            expanded: false,
            group_expanded: false,
        }));

        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(app.selected_tool, Some(0));

        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let TranscriptItem::Tool(run) = &app.transcript[0] else {
            panic!("expected tool run");
        };
        assert!(run.expanded);

        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert_eq!(app.selected_tool, None);
        let TranscriptItem::Tool(run) = &app.transcript[0] else {
            panic!("expected tool run");
        };
        assert!(!run.expanded);
    }

    #[test]
    fn open_selected_tool_renders_inline_output() {
        let transcript = vec![TranscriptItem::Tool(ToolRun {
            id: None,
            started_at: Instant::now(),
            pending_result: None,
            name: "terminal.exec".to_string(),
            summary: "$ cargo test -p medusa-tui".to_string(),
            state: ToolRunState::Succeeded,
            detail: "29 passed\n2 ignored".to_string(),
            expanded: true,
            group_expanded: false,
        })];

        let lines = visible_transcript_lines(&transcript, None, Some(0));

        let text = lines.iter().map(line_text).collect::<Vec<_>>();
        assert!(text.iter().any(|line| line.contains("⎿ 29 passed")));
        assert!(text.iter().any(|line| line.contains("2 ignored")));
    }

    fn type_chars(app: &mut App, text: &str) {
        for ch in text.chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
    }

    #[test]
    fn collect_workspace_files_skips_junk_dirs_and_respects_cap() {
        let workspace = temp_workspace();
        fs::create_dir_all(workspace.join("src")).unwrap();
        fs::create_dir_all(workspace.join(".git")).unwrap();
        fs::create_dir_all(workspace.join("target/debug")).unwrap();
        fs::create_dir_all(workspace.join("node_modules/pkg")).unwrap();
        fs::write(workspace.join("README.md"), "x").unwrap();
        fs::write(workspace.join("src/main.rs"), "x").unwrap();
        fs::write(workspace.join(".git/config"), "x").unwrap();
        fs::write(workspace.join("target/debug/junk"), "x").unwrap();
        fs::write(workspace.join("node_modules/pkg/index.js"), "x").unwrap();

        let files = collect_workspace_files(&workspace, MENTION_FILE_WALK_CAP);
        assert_eq!(
            files,
            vec!["README.md".to_string(), "src/main.rs".to_string()]
        );

        for index in 0..10 {
            fs::write(workspace.join(format!("file{index}.txt")), "x").unwrap();
        }
        assert_eq!(collect_workspace_files(&workspace, 3).len(), 3);
    }

    #[test]
    fn mention_match_prefers_file_name_then_segment_then_substring() {
        let (name_score, positions) =
            mention_match("crates/medusa-tui/src/main.rs", "main").unwrap();
        assert_eq!(name_score, 0);
        assert_eq!(positions, (22..26).collect::<Vec<_>>());

        let (segment_score, _) = mention_match("crates/medusa-tui/src/main.rs", "medusa").unwrap();
        assert_eq!(segment_score, 1);

        let (substring_score, _) = mention_match("docs/comments.md", "men").unwrap();
        assert_eq!(substring_score, 2);

        let (subsequence_score, _) = mention_match("src/handlers.rs", "hdl").unwrap();
        assert_eq!(subsequence_score, 3);

        assert!(mention_match("src/lib.rs", "zzz").is_none());
    }

    #[test]
    fn typing_at_token_opens_mention_picker_and_enter_inserts_path() {
        let (mut app, workspace) = app_in_workspace();
        fs::create_dir_all(workspace.join("src")).unwrap();
        fs::write(workspace.join("src/lib.rs"), "x").unwrap();
        fs::write(workspace.join("README.md"), "x").unwrap();

        type_chars(&mut app, "look at @li");
        assert!(app.mention_popup_visible());
        let matches = app.mention_matches();
        assert_eq!(matches[0].0, "src/lib.rs");

        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.input, "look at src/lib.rs ");
        assert_eq!(app.input_cursor, app.input_len());
        assert!(!app.mention_popup_visible());
        // The transcript got no user message: Enter completed, not submitted.
        assert!(app.transcript.is_empty());
    }

    #[test]
    fn tab_completes_mention_and_up_down_navigate() {
        let mut app = app();
        // Pre-seeding the candidate list keeps the walk out of this test:
        // refresh_mention_state only loads files when none are cached.
        app.mention_files = Some(vec![
            "src/alpha.rs".to_string(),
            "src/alpine.rs".to_string(),
        ]);
        type_chars(&mut app, "@alp");
        assert!(app.mention_popup_visible());
        assert_eq!(app.mention_selection, 0);

        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.mention_selection, 1);
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.mention_selection, 0);

        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.input, "src/alpha.rs ");
    }

    #[test]
    fn escape_dismisses_mention_picker_without_clearing_input() {
        let (mut app, workspace) = app_in_workspace();
        fs::write(workspace.join("notes.txt"), "x").unwrap();

        type_chars(&mut app, "@not");
        assert!(app.mention_popup_visible());

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!app.mention_popup_visible());
        assert_eq!(app.input, "@not");

        // Typing again reopens the picker.
        type_chars(&mut app, "e");
        assert!(app.mention_popup_visible());
    }

    #[test]
    fn mention_picker_defers_to_slash_suggestions() {
        let mut app = app();
        app.mention_files = Some(vec!["help.md".to_string()]);
        type_chars(&mut app, "/he");
        assert!(app.slash_suggestions_active());
        assert!(!app.mention_popup_visible());
        assert!(app.mention_matches().is_empty());
    }

    #[test]
    fn mention_picker_wins_over_decision_until_dismissed() {
        let (mut app, workspace) = app_in_workspace();
        fs::write(workspace.join("plan.md"), "x").unwrap();
        app.apply_decision_request_output(
            r#"{"title":"pick","reason":"","questions":[{"id":"q1","prompt":"which?","kind":"text"}]}"#,
        )
        .unwrap();
        assert!(app.pending_decision().is_some());

        type_chars(&mut app, "@pla");
        assert!(app.mention_popup_visible());

        // Enter completes the mention, not the decision answer.
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.input, "plan.md ");
        assert!(app.pending_decision().is_some());

        // With the picker gone, Enter answers the decision with the text.
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.pending_decision().is_none());
    }

    #[test]
    fn mention_picker_leaves_multiline_navigation_alone() {
        let (mut app, workspace) = app_in_workspace();
        fs::write(workspace.join("data.csv"), "x").unwrap();

        app.input = "first\nsecond".to_string();
        app.input_cursor = app.input_len();
        assert!(!app.mention_popup_visible());
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.input_cursor, 5); // moved to line 1, column 5

        // With an active mention token, Up drives the picker instead.
        app.input_cursor = app.input_len();
        type_chars(&mut app, " @dat");
        assert!(app.mention_popup_visible());
        let cursor_before = app.input_cursor;
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.input_cursor, cursor_before);
    }

    #[test]
    fn quick_memory_content_creates_file_section_and_appends() {
        let created = quick_memory_content("", "use rg not grep");
        assert_eq!(
            created,
            "# Project notes\n\n## Notes\n\n- use rg not grep\n"
        );

        let appended = quick_memory_content(&created, "tests live in-module");
        assert_eq!(
            appended,
            "# Project notes\n\n## Notes\n\n- use rg not grep\n- tests live in-module\n"
        );

        let with_following_section =
            "# Repo\n\n## Notes\n\n- old note\n\n## Commands\n\ncargo test\n";
        let inserted = quick_memory_content(with_following_section, "new note");
        assert_eq!(
            inserted,
            "# Repo\n\n## Notes\n\n- old note\n- new note\n\n## Commands\n\ncargo test\n"
        );

        let no_section = "# Repo\n\nSome intro.\n";
        let grown = quick_memory_content(no_section, "first note");
        assert_eq!(grown, "# Repo\n\nSome intro.\n\n## Notes\n\n- first note\n");
    }

    /// [22]: a multi-line quick-memory note containing a markdown heading must
    /// not forge a new AGENTS.md section or scramble later insertions.
    #[test]
    fn quick_memory_neutralizes_multiline_heading_notes() {
        let existing = "# Repo\n\n## Notes\n\n- old note\n";
        let note = "deploy steps\n## Deploy\nrun make ship";
        let content = quick_memory_content(existing, note);

        // No line inside the file is a forged heading carrying the pasted text.
        assert!(
            !content
                .lines()
                .any(|line| line.trim_start().starts_with('#') && line.contains("Deploy")),
            "pasted heading forged a section:\n{content}"
        );
        // The note collapsed to a single bullet under Notes.
        assert!(
            content.contains("- deploy steps ## Deploy run make ship"),
            "note not collapsed to one bullet:\n{content}"
        );

        // A later note still lands under the same Notes section, in order, and
        // no rogue standalone "## Deploy" heading exists to break the scan.
        let next = quick_memory_content(&content, "second note");
        let notes_at = next.find("## Notes").unwrap();
        let old_at = next.find("- old note").unwrap();
        let first_at = next.find("- deploy steps").unwrap();
        let second_at = next.find("- second note").unwrap();
        assert!(
            notes_at < old_at && old_at < first_at && first_at < second_at,
            "later note escaped the Notes section:\n{next}"
        );
        assert!(
            !next.lines().any(|line| line.trim() == "## Deploy"),
            "a standalone Deploy heading was forged:\n{next}"
        );
    }

    /// [22]: a note that *starts* with a heading marker has it neutralized so
    /// the bullet can never itself read as a heading.
    #[test]
    fn quick_memory_strips_leading_heading_marker() {
        let content = quick_memory_content("", "## Deploy\nrun make ship");
        assert!(
            content.contains("- Deploy run make ship"),
            "leading heading marker not neutralized:\n{content}"
        );
        assert!(
            !content.lines().any(|line| line.trim() == "## Deploy"),
            "leading heading marker forged a section:\n{content}"
        );
    }

    #[test]
    fn hash_note_writes_agents_md_without_sending_a_turn() {
        let (mut app, workspace) = app_in_workspace();
        app.input = "# always run clippy before finishing".to_string();
        app.input_cursor = app.input_len();

        app.submit_input();

        let content = fs::read_to_string(workspace.join("AGENTS.md")).unwrap();
        assert!(content.contains("## Notes"));
        assert!(content.contains("- always run clippy before finishing"));

        // Nothing went to the model: no user message, no worker channel.
        assert!(app.model_events.is_none());
        assert!(!app.transcript.iter().any(|item| matches!(
            item,
            TranscriptItem::Message(ChatMessage {
                role: ChatRole::User,
                ..
            })
        )));

        // Muted system line explains when the note takes effect.
        let Some(TranscriptItem::Message(message)) = app.transcript.last() else {
            panic!("expected transcript line");
        };
        assert_eq!(message.role, ChatRole::System);
        assert!(message.content.contains("applies from next turn"));

        assert_eq!(app.input, "");
        let toast = app.toast.as_ref().expect("toast");
        assert_eq!(toast.message, "noted in AGENTS.md");
        assert_eq!(toast.kind, ToastKind::Success);

        // The per-turn project-instructions loader picks the note up, which
        // is what carries it to the model next turn.
        let context = medusa_core::project::project_instructions_context(&workspace).unwrap();
        assert!(context.contains("always run clippy before finishing"));

        // A second note appends instead of duplicating the section.
        app.input = "# prefer eyre::Result".to_string();
        app.input_cursor = app.input_len();
        app.submit_input();
        let content = fs::read_to_string(workspace.join("AGENTS.md")).unwrap();
        assert_eq!(content.matches("## Notes").count(), 1);
        assert!(content.contains("- prefer eyre::Result"));
    }

    #[test]
    fn bell_gating_requires_enabled_and_long_turn() {
        assert!(!should_ring_bell(true, None));
        assert!(!should_ring_bell(true, Some(Duration::from_secs(3))));
        assert!(!should_ring_bell(false, Some(Duration::from_secs(30))));
        assert!(should_ring_bell(true, Some(Duration::from_secs(11))));
    }

    #[test]
    fn bell_env_override_beats_setting() {
        assert!(bell_enabled(true, None));
        assert!(!bell_enabled(false, None));
        assert!(!bell_enabled(true, Some("off")));
        assert!(!bell_enabled(true, Some("OFF")));
        assert!(!bell_enabled(true, Some("0")));
        assert!(bell_enabled(false, Some("on")));
        assert!(bell_enabled(true, Some("unrecognized")));
    }

    #[test]
    fn bell_preference_round_trips_through_settings() {
        let workspace = temp_workspace();
        save_bell_preference(&workspace, false).unwrap();
        let settings = load_app_settings(&workspace).unwrap();
        assert_eq!(settings.bell, Some(false));
    }
}
