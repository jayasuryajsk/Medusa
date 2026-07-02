use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    env, fs, io,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::{
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
        Block, BorderType, Borders, Cell, Clear, Gauge, List, ListItem, ListState, Padding,
        Paragraph, Row, Scrollbar, ScrollbarOrientation, ScrollbarState, Table, Tabs, Wrap,
    },
};
use ratatui_image::{
    Resize,
    picker::Picker,
    sliced::{SignedPosition, SlicedImage, SlicedProtocol},
};
use serde::{Deserialize, Serialize};

use medusa_core::auth::probe_codex_auth;
use medusa_core::model::{
    ConversationAttachment, ConversationMessage, DirectCodexBackend, ModelStreamEvent,
};
use medusa_core::permissions::{PermissionMode, PermissionPolicy};
use medusa_core::session::{
    SessionOpenMode, SessionStore as CoreSessionStore, compact_session_id, human_bytes,
};
use medusa_core::tools::{
    BackgroundJobEvent, FilePatchRequest, TaskUpdateRequest, TerminalExecRequest,
    TerminalExecResult, ToolRuntime,
};
use medusa_core::workflow::{
    SubagentToolPolicy, WorkflowEvent, WorkflowPhasePlan, WorkflowRuntime, WorkflowStatus,
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
        ModelStreamEvent::ToolStart { name, summary } => {
            if !options.json {
                eprintln!("tool start: {name} · {}", compact_one_line(&summary, 180));
            }
            tools_seen.push(HeadlessToolEvent {
                name,
                summary,
                failed: None,
            });
        }
        ModelStreamEvent::ToolResult { name, output } => {
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
        ModelStreamEvent::Done { .. } | ModelStreamEvent::Error(_) => {}
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

fn save_permission_mode_preference(workspace: &Path, mode: PermissionMode) -> Result<()> {
    let mut settings = load_app_settings(workspace).unwrap_or_default();
    settings.permission_mode = Some(mode.name().to_string());
    save_app_settings(workspace, &settings)?;
    PermissionPolicy::write_mode(workspace, mode)
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

#[derive(Debug)]
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
    last_transcript_rows: Vec<TranscriptRow>,
    should_quit: bool,
    restart_requested: bool,
    cwd_display: String,
    inside_git_repo: bool,
    theme: ThemeKind,
    permission_mode: PermissionMode,
    tools: ToolRuntime,
    model: DirectCodexBackend,
    model_enabled: bool,
    model_events: Option<Receiver<ModelStreamEvent>>,
    workflow_events: Vec<Receiver<WorkflowEvent>>,
    background_job_sender: Sender<BackgroundJobEvent>,
    background_job_events: Receiver<BackgroundJobEvent>,
    background_jobs: BTreeMap<String, BackgroundJobView>,
    streaming_message: Option<usize>,
    queued_turns: VecDeque<String>,
    last_stream_save: Instant,
    chat_scroll: usize,
    chat_scroll_target: usize,
    selected_tool: Option<usize>,
    activity_tools: Vec<ToolRun>,
    activity_phase: ToolActivityPhase,
    activity_selected: Option<usize>,
    activity_detail_tab: ToolDetailTab,
    activity_detail_scroll: usize,
    plan_tab: PlanTab,
    plan_scroll: usize,
    decision_selection: usize,
    workflows: Vec<WorkflowRunView>,
    sidebar_width: u16,
    resizing_sidebar: bool,
    workspace_area: Option<Rect>,
    sidebar_divider_x: Option<u16>,
    animation_tick: u64,
    started_at: Instant,
    turn_started_at: Option<Instant>,
    session: Option<SessionStore>,
    active_modal: Option<Modal>,
    slash_selection: usize,
    settings_selection: usize,
    model_selection: usize,
    permission_selection: usize,
    theme_selection: usize,
    image_preview_index: usize,
    image_preview_zoom: u16,
    theme_preview_original: Option<ThemeKind>,
    toast: Option<Toast>,
}

const DEFAULT_SIDEBAR_WIDTH: u16 = 38;
const MIN_SIDEBAR_WIDTH: u16 = 24;
const MAX_SIDEBAR_WIDTH: u16 = 72;
const MIN_CHAT_WIDTH_WITH_SIDEBAR: u16 = 44;
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
const CONTEXT_RECENT_MESSAGES: usize = 24;
const CONTEXT_RECENT_MAX_CHARS: usize = 48_000;
const SESSION_STATE_MAX_INTENTS: usize = 8;
const SESSION_STATE_MAX_OUTCOMES: usize = 8;
const SESSION_STATE_MAX_SYSTEM_NOTES: usize = 6;
const SESSION_STATE_MAX_TOOLS: usize = 12;
const SESSION_STATE_MAX_FILES: usize = 16;
const SESSION_MEMORY_MAX_PER_KIND: usize = 5;

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
}

const THEME_KINDS: [ThemeKind; 13] = [
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
        env::var("MEDUSA_THEME")
            .ok()
            .and_then(|value| Self::from_name(&value))
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

fn model_choices(current: &str) -> Vec<String> {
    let mut choices = DEFAULT_MODEL_CHOICES
        .iter()
        .map(|model| (*model).to_string())
        .collect::<Vec<_>>();
    let current = current.trim();
    if !current.is_empty() && !choices.iter().any(|model| model == current) {
        choices.insert(0, current.to_string());
    }
    choices
}

fn model_index(current: &str) -> usize {
    model_choices(current)
        .iter()
        .position(|model| model == current)
        .unwrap_or(0)
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
    Activity,
    Plan,
    ImagePreview,
    Workflows,
    Jobs,
    Sessions,
    SessionTree,
    Models,
    Permissions,
    Themes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolActivityPhase {
    Idle,
    CurrentTurn,
    LastTurn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolDetailTab {
    Summary,
    Output,
    Timeline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlanTab {
    Plan,
    History,
    Blockers,
    Evidence,
}

impl PlanTab {
    fn all() -> &'static [Self] {
        &[Self::Plan, Self::History, Self::Blockers, Self::Evidence]
    }

    fn label(self) -> &'static str {
        match self {
            Self::Plan => "Plan",
            Self::History => "History",
            Self::Blockers => "Blockers",
            Self::Evidence => "Evidence",
        }
    }

    fn index(self) -> usize {
        Self::all().iter().position(|tab| *tab == self).unwrap_or(0)
    }

    fn at_offset(self, offset: isize) -> Self {
        let tabs = Self::all();
        let next = (self.index() as isize + offset).rem_euclid(tabs.len() as isize) as usize;
        tabs[next]
    }
}

impl ToolDetailTab {
    fn all() -> &'static [Self] {
        &[Self::Summary, Self::Output, Self::Timeline]
    }

    fn label(self) -> &'static str {
        match self {
            Self::Summary => "Summary",
            Self::Output => "Output",
            Self::Timeline => "Timeline",
        }
    }

    fn index(self) -> usize {
        Self::all().iter().position(|tab| *tab == self).unwrap_or(0)
    }

    fn at_offset(self, offset: isize) -> Self {
        let tabs = Self::all();
        let next = (self.index() as isize + offset).rem_euclid(tabs.len() as isize) as usize;
        tabs[next]
    }
}

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

#[derive(Debug, Clone, Copy)]
struct RenderContext {
    animation_tick: u64,
}

impl RenderContext {
    #[cfg(test)]
    fn static_view() -> Self {
        Self { animation_tick: 0 }
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
}

#[derive(Debug, Clone)]
struct TranscriptRowsCache {
    version: u64,
    theme: ThemeKind,
    streaming_message: Option<usize>,
    selected_tool: Option<usize>,
    animation_tick: Option<u64>,
    rows: Vec<TranscriptRow>,
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
        Self::build(model_enabled, None)
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
        let cwd = env::current_dir().unwrap_or_else(|_| Path::new(".").to_path_buf());
        let tools = ToolRuntime::new(&cwd).expect("current directory should be usable");
        let app_settings = load_app_settings(tools.workspace()).unwrap_or_default();
        let mut model =
            DirectCodexBackend::new(tools.workspace().to_path_buf()).expect("HTTP client builds");
        if env::var_os("MEDUSA_MODEL").is_none()
            && let Some(model_name) = app_settings.model()
        {
            model.set_model_name(model_name);
        }
        let cwd_display = abbreviate_home(&tools.workspace().to_string_lossy());
        let inside_git_repo = Path::new(".git").exists();
        let theme = ThemeKind::from_workspace_settings(tools.workspace());
        let permission_mode = app_settings.permission_mode();
        set_active_theme(theme);
        let (background_job_sender, background_job_events) = mpsc::channel();

        Self {
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
            last_transcript_rows: Vec::new(),
            should_quit: false,
            restart_requested: false,
            cwd_display,
            inside_git_repo,
            theme,
            permission_mode,
            tools,
            model,
            model_enabled,
            model_events: None,
            workflow_events: Vec::new(),
            background_job_sender,
            background_job_events,
            background_jobs: BTreeMap::new(),
            streaming_message: None,
            queued_turns: VecDeque::new(),
            last_stream_save: Instant::now(),
            chat_scroll: 0,
            chat_scroll_target: 0,
            selected_tool: None,
            activity_tools: Vec::new(),
            activity_phase: ToolActivityPhase::Idle,
            activity_selected: None,
            activity_detail_tab: ToolDetailTab::Summary,
            activity_detail_scroll: 0,
            plan_tab: PlanTab::Plan,
            plan_scroll: 0,
            decision_selection: 0,
            workflows: Vec::new(),
            sidebar_width: DEFAULT_SIDEBAR_WIDTH,
            resizing_sidebar: false,
            workspace_area: None,
            sidebar_divider_x: None,
            animation_tick: 0,
            started_at: Instant::now(),
            turn_started_at: None,
            session,
            active_modal: None,
            slash_selection: 0,
            settings_selection: 0,
            model_selection: 0,
            permission_selection: permission_mode_index(permission_mode),
            theme_selection: theme_index(theme),
            image_preview_index: 0,
            image_preview_zoom: 100,
            theme_preview_original: None,
            toast: None,
        }
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
            let pending_tool_changed = self.drain_pending_tool_results();

            needs_draw |= terminal_changed
                || toast_changed
                || model_changed
                || workflow_changed
                || background_changed
                || pending_tool_changed
                || animation_changed;

            let frame_cadence = if animated {
                Duration::from_millis(16)
            } else {
                Duration::from_millis(50)
            };

            if needs_draw && (terminal_changed || last_draw.elapsed() >= frame_cadence) {
                terminal.draw(|frame| self.draw(frame))?;
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

            if self.active_modal == Some(Modal::Activity) {
                match key.code {
                    KeyCode::Esc => {
                        self.active_modal = None;
                        self.status_line = "closed".to_string();
                    }
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        self.should_quit = true;
                    }
                    KeyCode::Char('j') | KeyCode::Down => self.select_next_activity_tool(),
                    KeyCode::Char('k') | KeyCode::Up => self.select_previous_activity_tool(),
                    KeyCode::Char('l') | KeyCode::Right | KeyCode::Tab => {
                        self.next_activity_detail_tab()
                    }
                    KeyCode::Char('h') | KeyCode::Left | KeyCode::BackTab => {
                        self.previous_activity_detail_tab()
                    }
                    KeyCode::PageDown => self.scroll_activity_detail_by(8),
                    KeyCode::PageUp => self.scroll_activity_detail_by(-8),
                    KeyCode::Home => self.scroll_activity_detail_home(),
                    KeyCode::End => self.scroll_activity_detail_end(),
                    KeyCode::Enter => self.toggle_activity_tool_detail(),
                    KeyCode::Char('x') => self.close_activity_tool_detail(),
                    _ => {}
                }
                return;
            }

            if self.active_modal == Some(Modal::Plan) {
                match key.code {
                    KeyCode::Esc => self.close_modal(),
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        self.should_quit = true;
                    }
                    KeyCode::Char('l') | KeyCode::Right | KeyCode::Tab => self.next_plan_tab(),
                    KeyCode::Char('h') | KeyCode::Left | KeyCode::BackTab => {
                        self.previous_plan_tab()
                    }
                    KeyCode::PageDown | KeyCode::Char('j') | KeyCode::Down => {
                        self.scroll_plan_by(6)
                    }
                    KeyCode::PageUp | KeyCode::Char('k') | KeyCode::Up => self.scroll_plan_by(-6),
                    KeyCode::Home => self.scroll_plan_home(),
                    KeyCode::End => self.scroll_plan_end(),
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
            KeyCode::Esc => self.should_quit = true,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true;
            }
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.open_activity_modal();
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
            KeyCode::Left if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.resize_sidebar_wider(2);
            }
            KeyCode::Right if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.resize_sidebar_narrower(2);
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
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.insert_input_char('\n');
            }
            KeyCode::Enter | KeyCode::Char('\n') | KeyCode::Char('\r')
                if self.slash_suggestions_active() =>
            {
                self.accept_slash_suggestion();
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
            KeyCode::Home if key.modifiers.is_empty() => self.input_cursor = 0,
            KeyCode::End if key.modifiers.is_empty() => self.input_cursor = self.input_len(),
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

    fn slash_matches(&self) -> Vec<&'static SlashCommand> {
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
                slash_match_score(command, &query).map(|score| (score, index, command))
            })
            .collect::<Vec<_>>();
        matches.sort_by_key(|(score, index, _)| (*score, *index));
        matches.into_iter().map(|(_, _, command)| command).collect()
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
        let Some(command) = self.slash_matches().get(self.slash_selection).copied() else {
            self.submit_input();
            return;
        };

        if command.name == "/theme" {
            self.input.clear();
            self.input_cursor = 0;
            self.open_themes_modal();
        } else if command.name == "/model" {
            self.input.clear();
            self.input_cursor = 0;
            self.open_models_modal();
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
            Some(_) | None => {
                self.status_line = "setting is read-only".to_string();
                self.toast("Read-only setting", ToastKind::Info);
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
            MouseEventKind::Down(MouseButton::Left) if self.mouse_on_sidebar_divider(mouse) => {
                self.resizing_sidebar = true;
                self.resize_sidebar_to_column(mouse.column);
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(attachment) = self.image_attachment_at_mouse(mouse) {
                    self.open_image_preview_for_attachment(&attachment);
                }
            }
            MouseEventKind::Drag(MouseButton::Left) if self.resizing_sidebar => {
                self.resize_sidebar_to_column(mouse.column);
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if self.resizing_sidebar {
                    self.resizing_sidebar = false;
                    self.status_line = format!("tool sidebar width: {}", self.sidebar_width);
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

    fn mouse_on_sidebar_divider(&self, mouse: MouseEvent) -> bool {
        let Some(workspace) = self.workspace_area else {
            return false;
        };
        if mouse.row < workspace.y || mouse.row >= workspace.y.saturating_add(workspace.height) {
            return false;
        }
        self.sidebar_divider_x
            .is_some_and(|x| mouse.column.abs_diff(x) <= 1)
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

    fn resize_sidebar_to_column(&mut self, column: u16) {
        let Some(workspace) = self.workspace_area else {
            return;
        };
        if workspace.width < 100 {
            return;
        }
        let right_edge = workspace.x.saturating_add(workspace.width);
        let raw_width = right_edge.saturating_sub(column);
        self.sidebar_width = self.clamp_sidebar_width(raw_width, workspace.width);
        self.status_line = format!("resizing tools: {}", self.sidebar_width);
    }

    fn resize_sidebar_wider(&mut self, amount: u16) {
        let workspace_width = self.workspace_area.map_or(120, |area| area.width);
        self.sidebar_width =
            self.clamp_sidebar_width(self.sidebar_width.saturating_add(amount), workspace_width);
        self.status_line = format!("tool sidebar width: {}", self.sidebar_width);
    }

    fn resize_sidebar_narrower(&mut self, amount: u16) {
        let workspace_width = self.workspace_area.map_or(120, |area| area.width);
        self.sidebar_width =
            self.clamp_sidebar_width(self.sidebar_width.saturating_sub(amount), workspace_width);
        self.status_line = format!("tool sidebar width: {}", self.sidebar_width);
    }

    fn clamp_sidebar_width(&self, width: u16, workspace_width: u16) -> u16 {
        let max_for_workspace = workspace_width.saturating_sub(MIN_CHAT_WIDTH_WITH_SIDEBAR);
        let max_width = MAX_SIDEBAR_WIDTH
            .min(max_for_workspace)
            .max(MIN_SIDEBAR_WIDTH);
        width.clamp(MIN_SIDEBAR_WIDTH, max_width)
    }

    fn sidebar_width_for_area(&self, area: Rect) -> u16 {
        self.clamp_sidebar_width(self.sidebar_width, area.width)
    }

    fn update_sidebar_geometry(&mut self, workspace: Rect, sidebar: Option<Rect>) {
        self.workspace_area = Some(workspace);
        self.sidebar_divider_x = sidebar.map(|area| area.x.saturating_sub(1));
        if workspace.width >= 100 {
            self.sidebar_width = self.sidebar_width_for_area(workspace);
        }
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
            self.status_line = "no activity cards".to_string();
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
        self.status_line = "activity card selected".to_string();
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
            self.status_line = "no activity card selected".to_string();
            return;
        };

        let next_expanded = !self.transcript[start..end]
            .iter()
            .any(|item| matches!(item, TranscriptItem::Tool(run) if run.expanded));

        for item in &mut self.transcript[start..end] {
            if let TranscriptItem::Tool(run) = item {
                run.expanded = false;
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
        } else {
            "tool details closed".to_string()
        };
        self.persist_session();
    }

    fn attach_or_push_background_tool_start(&mut self, id: &str, command: &str) {
        let summary = format!("$ {command}");
        if let Some(run) = self.activity_tools.iter_mut().rev().find(|run| {
            run.id.is_none()
                && run.name == "terminal.exec"
                && run.summary == summary
                && run.state == ToolRunState::Running
        }) {
            run.id = Some(id.to_string());
        } else {
            self.push_tool_start_with_id(
                Some(id.to_string()),
                "terminal.exec".to_string(),
                summary.clone(),
            );
        }
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
        }
    }

    fn update_tool_result_by_id(&mut self, id: &str, state: ToolRunState, detail: &str) {
        let detail = compact_tool_detail(detail);
        for run in self
            .activity_tools
            .iter_mut()
            .rev()
            .filter(|run| run.id.as_deref() == Some(id))
            .take(1)
        {
            queue_or_apply_tool_result(run, state, detail.clone(), state == ToolRunState::Failed);
        }
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
                    changed = true;
                }
            }
        }

        if changed {
            self.touch_transcript();
        }
        self.selected_tool = None;
        self.status_line = "activity card closed".to_string();
        self.persist_session();
    }

    fn open_activity_modal(&mut self) {
        if !self.activity_tools.is_empty() && self.activity_selected.is_none() {
            self.activity_selected = Some(0);
        }
        self.active_modal = Some(Modal::Activity);
        self.status_line = "activity opened".to_string();
    }

    fn open_plan_modal(&mut self) {
        self.active_modal = Some(Modal::Plan);
        self.plan_scroll = 0;
        self.status_line = if let Some(plan) = self.current_plan() {
            let progress = plan_progress(plan);
            format!(
                "plan opened · {} steps · {} done",
                plan.items.len(),
                progress.done
            )
        } else {
            "plan opened · no plan yet".to_string()
        };
    }

    fn next_plan_tab(&mut self) {
        self.plan_tab = self.plan_tab.at_offset(1);
        self.plan_scroll = 0;
        self.status_line = format!("plan {}", self.plan_tab.label().to_ascii_lowercase());
    }

    fn previous_plan_tab(&mut self) {
        self.plan_tab = self.plan_tab.at_offset(-1);
        self.plan_scroll = 0;
        self.status_line = format!("plan {}", self.plan_tab.label().to_ascii_lowercase());
    }

    fn scroll_plan_by(&mut self, amount: isize) {
        let max = self.plan_modal_max_scroll();
        if amount.is_negative() {
            self.plan_scroll = self.plan_scroll.saturating_sub(amount.unsigned_abs());
        } else {
            self.plan_scroll = self.plan_scroll.saturating_add(amount as usize).min(max);
        }
        self.status_line = "plan scroll".to_string();
    }

    fn scroll_plan_home(&mut self) {
        self.plan_scroll = 0;
        self.status_line = "plan top".to_string();
    }

    fn scroll_plan_end(&mut self) {
        self.plan_scroll = self.plan_modal_max_scroll();
        self.status_line = "plan bottom".to_string();
    }

    fn plan_modal_max_scroll(&self) -> usize {
        plan_modal_lines(self).len().saturating_sub(8)
    }

    fn current_plan(&self) -> Option<&PlanView> {
        self.transcript.iter().rev().find_map(|item| match item {
            TranscriptItem::Plan(plan) => Some(plan),
            _ => None,
        })
    }

    fn plan_history(&self) -> Vec<&PlanView> {
        self.transcript
            .iter()
            .filter_map(|item| match item {
                TranscriptItem::Plan(plan) => Some(plan),
                _ => None,
            })
            .collect()
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

    fn planning_rail_visible(&self, area: Rect) -> bool {
        area.width >= 96 && (self.current_plan().is_some() || self.current_decision().is_some())
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
        if self.pending_decision().is_none() || self.slash_suggestions_active() {
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

    fn select_next_activity_tool(&mut self) {
        if self.activity_tools.is_empty() {
            self.status_line = "no tool activity".to_string();
            return;
        }

        let next = self
            .activity_selected
            .map(|index| (index + 1) % self.activity_tools.len())
            .unwrap_or(0);
        self.activity_selected = Some(next);
        self.activity_detail_scroll = 0;
        self.status_line = format!("selected {}", self.activity_tools[next].name);
    }

    fn select_previous_activity_tool(&mut self) {
        if self.activity_tools.is_empty() {
            self.status_line = "no tool activity".to_string();
            return;
        }

        let next = self
            .activity_selected
            .map(|index| {
                if index == 0 {
                    self.activity_tools.len() - 1
                } else {
                    index - 1
                }
            })
            .unwrap_or_else(|| self.activity_tools.len() - 1);
        self.activity_selected = Some(next);
        self.activity_detail_scroll = 0;
        self.status_line = format!("selected {}", self.activity_tools[next].name);
    }

    fn next_activity_detail_tab(&mut self) {
        self.activity_detail_tab = self.activity_detail_tab.at_offset(1);
        self.activity_detail_scroll = 0;
        self.status_line = format!("activity tab: {}", self.activity_detail_tab.label());
    }

    fn previous_activity_detail_tab(&mut self) {
        self.activity_detail_tab = self.activity_detail_tab.at_offset(-1);
        self.activity_detail_scroll = 0;
        self.status_line = format!("activity tab: {}", self.activity_detail_tab.label());
    }

    fn scroll_activity_detail_by(&mut self, amount: isize) {
        if amount.is_negative() {
            self.activity_detail_scroll = self
                .activity_detail_scroll
                .saturating_sub(amount.unsigned_abs());
        } else {
            self.activity_detail_scroll =
                self.activity_detail_scroll.saturating_add(amount as usize);
        }
        self.status_line = "activity detail scroll".to_string();
    }

    fn scroll_activity_detail_home(&mut self) {
        self.activity_detail_scroll = 0;
        self.status_line = "activity detail top".to_string();
    }

    fn scroll_activity_detail_end(&mut self) {
        self.activity_detail_scroll = usize::MAX / 2;
        self.status_line = "activity detail bottom".to_string();
    }

    fn toggle_activity_tool_detail(&mut self) {
        let Some(index) = self.activity_selected else {
            self.status_line = "no tool selected".to_string();
            return;
        };

        if let Some(run) = self.activity_tools.get_mut(index) {
            run.expanded = !run.expanded;
            self.status_line = if run.expanded {
                "tool details open".to_string()
            } else {
                "tool details closed".to_string()
            };
            self.activity_detail_scroll = 0;
        }
    }

    fn close_activity_tool_detail(&mut self) {
        let Some(index) = self.activity_selected else {
            self.status_line = "no tool selected".to_string();
            return;
        };

        if let Some(run) = self.activity_tools.get_mut(index) {
            run.expanded = false;
        }
        self.status_line = "tool details closed".to_string();
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
            } else {
                self.status_line = "Type a task first.".to_string();
            }
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

        self.activity_tools.clear();
        self.activity_phase = ToolActivityPhase::CurrentTurn;
        self.activity_selected = None;

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

        if task == "/recap" {
            self.transcript
                .push(TranscriptItem::Message(ChatMessage::system(
                    self.recap_text(),
                )));
            self.touch_transcript();
            self.status_line = "recap added".to_string();
            self.toast("Session recap added", ToastKind::Info);
            return true;
        }

        if task == "/demo" {
            self.run_demo();
            return true;
        }

        if task == "/activity" {
            self.open_activity_modal();
            return true;
        }

        if task == "/plan" {
            self.open_plan_modal();
            return true;
        }

        if task == "/images" || task == "/image" {
            self.open_latest_image_preview();
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
            self.transcript
                .push(TranscriptItem::Message(ChatMessage::system(
                    "usage: /workflow <task>\n\nUse workflows for larger tasks that benefit from mapper, implementer, and verifier subagents.",
                )));
            self.touch_transcript();
            self.status_line = "workflow needs a task".to_string();
            self.toast("Workflow task required", ToastKind::Warning);
            return true;
        }

        if let Some(workflow_task) = task.strip_prefix("/workflow ") {
            self.start_workflow(workflow_task.trim());
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

        if task == "/clear" {
            self.transcript.clear();
            self.touch_transcript();
            self.selected_tool = None;
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

        if task == "/themes" || task == "/theme" {
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
                    self.tools.file_patch(FilePatchRequest::new(diff))
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

        if let Some(status) = task.strip_prefix("/status ") {
            match self
                .tools
                .task_update(TaskUpdateRequest::new(status.to_string()))
            {
                Ok(result) => {
                    self.status_line = result.status;
                }
                Err(error) => {
                    self.transcript
                        .push(TranscriptItem::Message(ChatMessage::system(format!(
                            "error: {error}"
                        ))));
                    self.touch_transcript();
                    self.status_line = "task.update failed".to_string();
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

    fn set_permission_mode(&mut self, mode: PermissionMode) {
        let workspace = self.tools.workspace().to_path_buf();
        self.permission_mode = mode;
        self.permission_selection = permission_mode_index(mode);
        self.status_line = format!("permissions: {}", mode.name());

        match save_permission_mode_preference(&workspace, mode)
            .and_then(|_| ToolRuntime::new(&workspace).map(|runtime| (runtime, ())))
        {
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
                self.touch_transcript();
                self.selected_tool = None;
                self.activity_tools.clear();
                self.activity_selected = None;
                self.activity_phase = ToolActivityPhase::Idle;
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
                self.activity_selected = None;
                self.status_line = format!("forked {name}");
                self.toast("Session forked", ToastKind::Success);
            }
            Err(error) => {
                self.status_line = format!("fork failed: {error}");
                self.toast("Fork failed", ToastKind::Error);
            }
        }
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

        self.activity_tools.clear();
        self.activity_phase = ToolActivityPhase::CurrentTurn;
        self.activity_selected = None;

        let assistant_index = self.transcript.len();
        self.transcript
            .push(TranscriptItem::Message(ChatMessage::assistant("")));
        self.touch_transcript();
        self.persist_session();
        self.streaming_message = Some(assistant_index);
        self.last_stream_save = Instant::now();
        self.status_line = self.scoped_status("streaming");
        self.turn_started_at = Some(Instant::now());

        let backend = self.model.clone();
        let permission_mode = self.permission_mode;
        let tools = self
            .tools
            .clone()
            .with_background_events(self.background_job_sender.clone());
        let prompt = self.conversation_history();
        let (sender, receiver) = mpsc::channel();
        self.model_events = Some(receiver);

        thread::spawn(move || {
            let result = if permission_mode == PermissionMode::Readonly {
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
                Err(error) => {
                    let _ = sender.send(ModelStreamEvent::Error(error.to_string()));
                }
            }
        });
    }

    fn is_working(&self) -> bool {
        self.model_events.is_some() || self.streaming_message.is_some()
    }

    fn has_active_workflows(&self) -> bool {
        !self.workflow_events.is_empty()
    }

    fn drain_pending_tool_results(&mut self) -> bool {
        let mut changed = false;
        for run in &mut self.activity_tools {
            if run.state == ToolRunState::Running
                && run.started_at.elapsed() >= MIN_TOOL_PULSE_VISIBLE
                && let Some(pending) = run.pending_result.take()
            {
                let expand = pending.state == ToolRunState::Failed;
                apply_tool_result_now(run, pending.state, pending.detail, expand);
                changed = true;
            }
        }

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
        self.last_transcript_rows.clear();
    }

    fn invalidate_render_cache(&mut self) {
        self.transcript_rows_cache = None;
        self.last_transcript_rows.clear();
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

        self.transcript
            .push(TranscriptItem::Message(ChatMessage::user(format!(
                "/workflow {}",
                task.trim()
            ))));
        self.touch_transcript();
        self.persist_session();
        self.scroll_chat_to_bottom();

        let runtime = WorkflowRuntime::new(self.tools.workspace().to_path_buf())
            .with_memory_context(self.session_state_context_text());
        let backend = self.model.clone();
        let tools = self.tools.clone();
        let task = task.trim().to_string();
        let (sender, receiver) = mpsc::channel();
        self.workflow_events.push(receiver);
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
                Ok(ModelStreamEvent::ToolStart { name, summary }) => {
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
                        self.push_tool_start(name.clone(), summary);
                        self.status_line = self.scoped_status(format!("running {name}"));
                    }
                    self.stick_chat_to_bottom_if_needed();
                }
                Ok(ModelStreamEvent::ToolResult { name, output }) => {
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
                        self.push_tool_result(&name, output);
                        self.status_line = self.scoped_status(format!("{name} complete"));
                    }
                    self.stick_chat_to_bottom_if_needed();
                }
                Ok(ModelStreamEvent::Done { event_count }) => {
                    changed = true;
                    self.flush_stream_delta(&mut delta_buffer);
                    self.status_line =
                        self.scoped_status(format!("complete ({event_count} events)"));
                    self.stick_chat_to_bottom_if_needed();
                    self.streaming_message = None;
                    self.finish_tool_activity_turn();
                    self.turn_started_at.take();
                    keep_receiver = false;
                    turn_finished = true;
                    break;
                }
                Ok(ModelStreamEvent::Error(error)) => {
                    changed = true;
                    self.flush_stream_delta(&mut delta_buffer);
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
                    self.finish_tool_activity_turn();
                    keep_receiver = false;
                    turn_finished = true;
                    break;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    changed = true;
                    self.flush_stream_delta(&mut delta_buffer);
                    if self.streaming_message.is_some() {
                        self.status_line = self.scoped_status("stream ended");
                    }
                    self.streaming_message = None;
                    self.finish_tool_activity_turn();
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
            self.start_next_queued_turn();
        }

        changed
    }

    fn drain_workflow_events(&mut self) -> bool {
        if self.workflow_events.is_empty() {
            return false;
        };

        let receivers = std::mem::take(&mut self.workflow_events);
        let mut active_receivers = Vec::new();
        let mut any_finished = false;
        let mut changed = false;

        for receiver in receivers {
            let mut keep_receiver = true;
            let mut processed = 0usize;
            let mut finished = false;

            while processed < 256 {
                match receiver.try_recv() {
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
                active_receivers.push(receiver);
            } else if finished {
                any_finished = true;
            }
        }

        self.workflow_events = active_receivers;
        if any_finished {
            self.persist_session();
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
                    if let Some(phase) = workflow.phases.get_mut(phase_index) {
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
                ..
            } => {
                self.update_workflow(&run_id, |workflow| {
                    workflow.status = WorkflowViewState::Running;
                    if let Some(agent) = workflow
                        .phases
                        .get_mut(phase_index)
                        .and_then(|phase| phase.agents.get_mut(agent_index))
                    {
                        agent.status = WorkflowViewState::Running;
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
                self.transcript
                    .push(TranscriptItem::Message(ChatMessage::assistant(
                        summary.clone(),
                    )));
                self.touch_transcript();
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

        self.activity_tools.clear();
        self.activity_phase = ToolActivityPhase::CurrentTurn;
        self.activity_selected = None;

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

    fn finish_tool_activity_turn(&mut self) {
        if self.activity_tools.is_empty() {
            self.activity_phase = ToolActivityPhase::Idle;
            self.activity_selected = None;
        } else {
            self.activity_phase = ToolActivityPhase::LastTurn;
            self.activity_selected = self.activity_selected.or(Some(0));
        }
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
        if self.activity_phase == ToolActivityPhase::Idle {
            self.activity_phase = ToolActivityPhase::CurrentTurn;
        }
        let run = ToolRun {
            id,
            started_at: Instant::now(),
            pending_result: None,
            name,
            summary,
            state: ToolRunState::Running,
            detail: String::new(),
            expanded: false,
        };
        self.transcript.push(TranscriptItem::Tool(run.clone()));
        self.touch_transcript();
        self.activity_tools.push(run);
        self.activity_selected = Some(self.activity_tools.len().saturating_sub(1));
        self.persist_session();
    }

    fn push_tool_result(&mut self, name: &str, output: String) {
        let state = if tool_output_failed(&output) {
            ToolRunState::Failed
        } else {
            ToolRunState::Succeeded
        };
        let detail = compact_tool_detail(&output);

        if let Some(run) = self
            .activity_tools
            .iter_mut()
            .rev()
            .find(|run| run.name == name && run.state == ToolRunState::Running)
        {
            queue_or_apply_tool_result(run, state, detail.clone(), false);
        } else {
            self.activity_tools.push(ToolRun {
                id: None,
                started_at: Instant::now(),
                pending_result: None,
                name: name.to_string(),
                summary: String::new(),
                state,
                detail: detail.clone(),
                expanded: false,
            });
        }
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

        messages.extend(self.recent_conversation_messages());
        messages
    }

    fn recent_conversation_messages(&self) -> Vec<ConversationMessage> {
        let mut recent = Vec::new();
        let mut used = 0usize;

        for item in self.transcript.iter().rev() {
            let Some(message) = transcript_conversation_message(item) else {
                continue;
            };
            let cost = conversation_message_cost(&message);
            if !recent.is_empty()
                && (recent.len() >= CONTEXT_RECENT_MESSAGES
                    || used.saturating_add(cost) > CONTEXT_RECENT_MAX_CHARS)
            {
                break;
            }
            used = used.saturating_add(cost);
            recent.push(message);
        }

        recent.reverse();
        recent
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
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Min(5),
                Constraint::Length(input_height),
                Constraint::Length(1),
            ])
            .split(shell_area);

        self.draw_header(frame, sections[0]);
        self.draw_workspace(frame, sections[1]);
        self.draw_input(frame, sections[2]);
        self.draw_status(frame, sections[3]);
        if self.active_modal.is_none() {
            self.draw_slash_suggestions(frame, shell_area);
        }
        self.draw_modal(frame, shell_area);
    }

    fn focus(&self) -> UiFocus {
        if self.active_modal == Some(Modal::Activity) {
            UiFocus::Activity
        } else if self.active_modal.is_some() {
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
        if self.planning_rail_visible(area) {
            let rail_width = self.sidebar_width_for_area(area).min(46);
            let chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Min(48), Constraint::Length(rail_width)])
                .split(area);
            self.update_sidebar_geometry(area, Some(chunks[1]));
            self.draw_messages(frame, chunks[0]);
            self.draw_planning_rail(frame, chunks[1]);
        } else {
            self.update_sidebar_geometry(area, None);
            self.draw_messages(frame, area);
        }
    }

    fn draw_planning_rail(&self, frame: &mut Frame<'_>, area: Rect) {
        if area.width < 8 || area.height == 0 {
            return;
        }

        let separator = Rect::new(area.x, area.y, 1, area.height);
        frame.render_widget(
            Paragraph::new(
                std::iter::repeat_n("│", area.height as usize)
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
            .style(Style::default().fg(palette().separator).bg(surface())),
            separator,
        );

        let inner = Rect::new(
            area.x.saturating_add(2),
            area.y,
            area.width.saturating_sub(3),
            area.height,
        );
        let lines = planning_rail_lines(self, inner.width, inner.height as usize);
        let rail = Paragraph::new(lines)
            .style(Style::default().bg(surface()).fg(text()))
            .wrap(Wrap { trim: true });
        frame.render_widget(rail, inner);
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

    fn draw_tool_activity_sidebar(&self, frame: &mut Frame<'_>, area: Rect) {
        let title = match self.activity_phase {
            ToolActivityPhase::Idle => " Activity ",
            ToolActivityPhase::CurrentTurn => " Activity: working ",
            ToolActivityPhase::LastTurn => " Activity: last turn ",
        };
        let border_style = if self
            .activity_tools
            .iter()
            .any(|run| run.state == ToolRunState::Failed)
        {
            error_style()
        } else if self
            .activity_tools
            .iter()
            .any(|run| run.state == ToolRunState::Running)
        {
            tool_label_style()
        } else if self.activity_tools.is_empty() {
            separator_style()
        } else {
            success_style()
        };
        let block = panel_block(title, self.focus() == UiFocus::Activity)
            .title(title)
            .border_style(border_style)
            .border_type(BorderType::Rounded)
            .padding(Padding::new(1, 1, 0, 0))
            .style(Style::default().bg(surface()).fg(text()));

        if self.activity_tools.is_empty() {
            frame.render_widget(block, area);
            let inner = area.inner(Margin {
                vertical: 1,
                horizontal: 2,
            });
            let message = match self.activity_phase {
                ToolActivityPhase::CurrentTurn => {
                    "Thinking… tool activity appears here when needed."
                }
                ToolActivityPhase::LastTurn => "No tools used last turn.",
                ToolActivityPhase::Idle => "Ask me to change or inspect code.",
            };
            let empty = Paragraph::new(vec![
                Line::from(Span::styled(message, muted())),
                Line::from(""),
                Line::from(Span::styled(
                    "drag edge resize · j/k select · enter details",
                    muted(),
                )),
            ])
            .wrap(Wrap { trim: true });
            frame.render_widget(empty, inner);
            return;
        }

        let selected = self
            .activity_selected
            .unwrap_or(0)
            .min(self.activity_tools.len().saturating_sub(1));
        let selected_expanded = self
            .activity_tools
            .get(selected)
            .is_some_and(|run| run.expanded);

        if selected_expanded && area.height >= 13 {
            let list_height = tool_activity_list_height(&self.activity_tools, area.width)
                .min(area.height / 2)
                .max(4);
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(list_height), Constraint::Min(5)])
                .split(area);
            self.draw_tool_activity_list(frame, chunks[0], Some(block));
            self.draw_tool_activity_detail(frame, chunks[1], selected);
        } else {
            self.draw_tool_activity_list(frame, area, Some(block));
        }
    }

    fn draw_tool_activity_list(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        block: Option<Block<'static>>,
    ) {
        let selected = self
            .activity_selected
            .map(|index| index.min(self.activity_tools.len().saturating_sub(1)));
        let inner = if block.is_some() {
            area.inner(Margin {
                vertical: 1,
                horizontal: 1,
            })
        } else {
            area
        };
        if let Some(block) = block {
            frame.render_widget(block, area);
        }

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(2), Constraint::Min(1)])
            .split(inner);

        let header = Paragraph::new(vec![
            tool_activity_header_line(self.activity_phase, &self.activity_tools),
            Line::from(Span::styled("", separator_style())),
        ])
        .style(Style::default().bg(surface()).fg(text()));
        frame.render_widget(header, chunks[0]);

        let items = self
            .activity_tools
            .iter()
            .enumerate()
            .map(|(index, run)| {
                tool_activity_item(
                    run,
                    selected == Some(index),
                    self.animation_tick,
                    chunks[1].width,
                )
            })
            .collect::<Vec<_>>();
        let mut state = ListState::default().with_selected(selected);
        let list = List::new(items)
            .style(Style::default().bg(surface()).fg(text()))
            .highlight_style(activity_selected_style())
            .highlight_symbol("▌ ");
        frame.render_stateful_widget(list, chunks[1], &mut state);

        if self.activity_tools.len() as u16 > chunks[1].height && chunks[1].width > 4 {
            let mut scrollbar_state =
                ScrollbarState::new(self.activity_tools.len()).position(selected.unwrap_or(0));
            frame.render_stateful_widget(activity_scrollbar(), chunks[1], &mut scrollbar_state);
        }
    }

    fn draw_tool_activity_detail(&self, frame: &mut Frame<'_>, area: Rect, index: usize) {
        let Some(run) = self.activity_tools.get(index) else {
            return;
        };
        let block = panel_block(" Tool details ", true)
            .title(" Tool details ")
            .border_style(tool_output_border_style())
            .border_type(BorderType::Rounded)
            .padding(Padding::new(1, 1, 0, 0))
            .style(Style::default().bg(surface()).fg(text()));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        if inner.height < 3 {
            return;
        }

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(1),
            ])
            .split(inner);

        let tab_titles = ToolDetailTab::all()
            .iter()
            .map(|tab| Line::from(Span::styled(tab.label(), muted())))
            .collect::<Vec<_>>();
        let tabs = Tabs::new(tab_titles)
            .select(self.activity_detail_tab.index())
            .highlight_style(tool_label_style().add_modifier(Modifier::BOLD))
            .style(Style::default().bg(surface()));
        frame.render_widget(tabs, chunks[0]);

        let hint = Paragraph::new(Line::from(vec![
            Span::styled("h/l", prompt_style()),
            Span::styled(" tab  ", muted()),
            Span::styled("pg", prompt_style()),
            Span::styled(" scroll  ", muted()),
            Span::styled("enter", prompt_style()),
            Span::styled(" collapse", muted()),
        ]))
        .style(Style::default().bg(surface()));
        frame.render_widget(hint, chunks[1]);

        match self.activity_detail_tab {
            ToolDetailTab::Summary => self.draw_tool_activity_summary(frame, chunks[2], run),
            ToolDetailTab::Output => self.draw_tool_activity_output(frame, chunks[2], run),
            ToolDetailTab::Timeline => self.draw_tool_activity_timeline(frame, chunks[2], run),
        }
    }

    fn draw_tool_activity_summary(&self, frame: &mut Frame<'_>, area: Rect, run: &ToolRun) {
        let card = tool_activity_card(run);
        let elapsed = run.started_at.elapsed();
        let output_lines = meaningful_tool_output_lines(run).len();
        let rows = [
            ("status", tool_state_label(run.state).to_string()),
            ("tool", run.name.clone()),
            ("action", card.action),
            ("target", card.target),
            ("elapsed", format!("{}s", elapsed.as_secs())),
            ("output", output_count_label(output_lines)),
        ]
        .into_iter()
        .map(|(key, value)| {
            let style = if key == "status" {
                tool_state_style(run.state)
            } else {
                value_style()
            };
            Row::new(vec![
                Cell::from(key).style(muted()),
                Cell::from(value).style(style),
            ])
        });

        let table = Table::new(rows, [Constraint::Length(10), Constraint::Min(20)])
            .style(Style::default().bg(surface()))
            .column_spacing(2);
        frame.render_widget(table, area);
    }

    fn draw_tool_activity_output(&self, frame: &mut Frame<'_>, area: Rect, run: &ToolRun) {
        let lines = tool_activity_output_lines(run, area.width);
        let max_scroll = lines.len().saturating_sub(area.height as usize);
        let scroll = self.activity_detail_scroll.min(max_scroll);
        let output = Paragraph::new(lines.clone())
            .style(Style::default().bg(surface()).fg(text()))
            .wrap(Wrap { trim: false })
            .scroll((scroll as u16, 0));
        frame.render_widget(output, area);

        if lines.len() > area.height as usize && area.width > 4 {
            let mut scrollbar_state = ScrollbarState::new(lines.len()).position(scroll);
            frame.render_stateful_widget(activity_scrollbar(), area, &mut scrollbar_state);
        }
    }

    fn draw_tool_activity_timeline(&self, frame: &mut Frame<'_>, area: Rect, run: &ToolRun) {
        let lines = tool_activity_timeline_lines(run);
        let max_scroll = lines.len().saturating_sub(area.height as usize);
        let scroll = self.activity_detail_scroll.min(max_scroll);
        let timeline = Paragraph::new(lines)
            .style(Style::default().bg(surface()).fg(text()))
            .wrap(Wrap { trim: false })
            .scroll((scroll as u16, 0));
        frame.render_widget(timeline, area);
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

        let height = area.height as usize;
        let total = metrics.total_visual_lines.max(1);
        let thumb_height = (height.saturating_mul(height) / total).clamp(1, height.min(5));
        let available = height.saturating_sub(thumb_height);
        let thumb_top = if metrics.max_scroll == 0 {
            0
        } else {
            metrics.top_offset.saturating_mul(available) / metrics.max_scroll
        };
        let lines = (0..height)
            .map(|row| {
                if row >= thumb_top && row < thumb_top.saturating_add(thumb_height) {
                    Line::from(Span::styled("▌", accent()))
                } else {
                    Line::from(" ")
                }
            })
            .collect::<Vec<_>>();
        let thumb_area = Rect::new(area.right().saturating_sub(1), area.y, 1, area.height);
        frame.render_widget(
            Paragraph::new(lines).style(Style::default().bg(surface())),
            thumb_area,
        );
    }

    fn visible_transcript_rows(&self) -> Vec<TranscriptRow> {
        visible_transcript_rows(
            &self.transcript,
            self.streaming_message,
            self.selected_tool,
            RenderContext {
                animation_tick: self.animation_tick,
            },
        )
    }

    fn visible_transcript_rows_cached(&mut self) -> Vec<TranscriptRow> {
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
        {
            return cache.rows.clone();
        }

        let rows = self.visible_transcript_rows();
        self.transcript_rows_cache = Some(TranscriptRowsCache {
            version: self.transcript_version,
            theme: self.theme,
            streaming_message: self.streaming_message,
            selected_tool: self.selected_tool,
            animation_tick,
            rows: rows.clone(),
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
                Constraint::Length(30),
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
        self.transcript
            .iter()
            .filter_map(|item| match item {
                TranscriptItem::Message(msg) => Some(msg.content.len()),
                TranscriptItem::Plan(plan) => Some(
                    plan.summary.len()
                        + plan
                            .items
                            .iter()
                            .map(|item| {
                                item.text.len()
                                    + item.evidence.iter().map(String::len).sum::<usize>()
                            })
                            .sum::<usize>(),
                ),
                TranscriptItem::Decision(decision) => Some(
                    decision.title.len()
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
                            .sum::<usize>(),
                ),
                _ => None,
            })
            .sum()
    }

    fn draw_context_gauge(&self, frame: &mut Frame<'_>, area: Rect) {
        if area.width < 4 {
            return;
        }
        const MAX_CHARS: usize = 120_000;
        let used = self.context_usage_chars();
        let ratio = (used as f64 / MAX_CHARS as f64).clamp(0.0, 1.0);
        let pct = (ratio * 100.0) as u16;
        let color = if ratio < 0.5 {
            palette().success
        } else if ratio < 0.75 {
            Color::Yellow
        } else {
            Color::Red
        };
        let gauge = Gauge::default()
            .gauge_style(Style::default().fg(color).bg(surface()))
            .percent(pct)
            .label(if area.width >= 8 {
                format!("{}%", pct)
            } else {
                String::new()
            })
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
        let text = format!(
            "tools {} · jobs {} · {activity}",
            self.activity_tools.len(),
            running
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

        Line::from(vec![
            Span::styled(" Message ", muted().add_modifier(Modifier::BOLD)),
            Span::styled(
                format!(" {} ", self.model.model_name()),
                accent().add_modifier(Modifier::BOLD),
            ),
        ])
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
            .map(|command| {
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{:<11}", command.name), prompt_style()),
                    Span::styled(format!("{:<9}", command.category), muted()),
                    Span::styled(command.args, value_style()),
                ]))
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

        let command = matches[selected];
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

    fn draw_modal(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let Some(modal) = self.active_modal else {
            return;
        };

        let popup_width = match modal {
            Modal::ImagePreview => area.width.saturating_sub(4).min(128),
            Modal::Settings | Modal::Themes => area.width.saturating_sub(8).min(94),
            Modal::Models | Modal::Permissions => area.width.saturating_sub(8).min(88),
            Modal::Plan => area.width.saturating_sub(8).min(104),
            _ => area.width.saturating_sub(8).min(78),
        };
        let popup_height = area.height.saturating_sub(4).min(match modal {
            Modal::Commands => 18,
            Modal::Settings => 18,
            Modal::Help => 17,
            Modal::Activity => 14,
            Modal::Plan => 20,
            Modal::ImagePreview => 36,
            Modal::Workflows => 18,
            Modal::Jobs => 16,
            Modal::Sessions => 14,
            Modal::SessionTree => 18,
            Modal::Models | Modal::Permissions => 16,
            Modal::Themes => 18,
        });
        let popup = centered_rect(area, popup_width, popup_height);
        frame.render_widget(Clear, popup);

        match modal {
            Modal::Commands => self.draw_commands_modal(frame, popup),
            Modal::Settings => self.draw_settings_modal(frame, popup),
            Modal::Help => self.draw_help_modal(frame, popup),
            Modal::Activity => self.draw_activity_modal(frame, popup),
            Modal::Plan => self.draw_plan_modal(frame, popup),
            Modal::ImagePreview => self.draw_image_preview_modal(frame, popup),
            Modal::Workflows => self.draw_workflows_modal(frame, popup),
            Modal::Jobs => self.draw_jobs_modal(frame, popup),
            Modal::Sessions => self.draw_sessions_modal(frame, popup),
            Modal::SessionTree => self.draw_session_tree_modal(frame, popup),
            Modal::Models => self.draw_models_modal(frame, popup),
            Modal::Permissions => self.draw_permissions_modal(frame, popup),
            Modal::Themes => self.draw_themes_modal(frame, popup),
        }
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
                ListItem::new(Line::from(vec![
                    Span::styled(
                        if active { "● " } else { "  " },
                        if active { success_style() } else { muted() },
                    ),
                    Span::styled(model.clone(), value_style()),
                ]))
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
            Line::from("Use /themes to inspect themes or /theme opencode to switch."),
            Line::from(""),
            Line::from(vec![Span::styled("Modals", prompt_style())]),
            Line::from("/plan opens the task checklist. /activity opens tool details."),
            Line::from("Esc or Enter closes simple popups."),
            Line::from(""),
            Line::from("Try /fork before risky work, or /tree to inspect branches."),
        ])
        .block(modal_block(" Help "))
        .wrap(Wrap { trim: false });
        frame.render_widget(help, area);
    }

    fn draw_activity_modal(&self, frame: &mut Frame<'_>, area: Rect) {
        if !self.activity_tools.is_empty() {
            self.draw_tool_activity_sidebar(frame, area);
            return;
        }

        let running_tools = self
            .transcript
            .iter()
            .filter(|item| matches!(item, TranscriptItem::Tool(run) if run.state == ToolRunState::Running))
            .count();
        let tool_count = self
            .transcript
            .iter()
            .filter(|item| matches!(item, TranscriptItem::Tool(_)))
            .count();
        let message_count = self
            .transcript
            .iter()
            .filter(|item| matches!(item, TranscriptItem::Message(_)))
            .count();

        let rows = vec![
            ("model", self.model.model_name().to_string()),
            (
                "stream",
                if self.is_working() { "active" } else { "idle" }.to_string(),
            ),
            ("queue", self.queued_turns.len().to_string()),
            (
                "tools",
                format!("{tool_count} total · {running_tools} running"),
            ),
            ("messages", message_count.to_string()),
            (
                "selected tool",
                self.selected_tool
                    .map(|i| i.to_string())
                    .unwrap_or_else(|| "none".to_string()),
            ),
            ("status", self.status_line.clone()),
        ];
        let rows = rows.into_iter().map(|(key, value)| {
            Row::new(vec![
                Cell::from(key).style(muted()),
                Cell::from(value).style(value_style()),
            ])
        });
        let table = Table::new(rows, [Constraint::Length(16), Constraint::Min(24)])
            .block(modal_block(" Activity "))
            .column_spacing(2);
        frame.render_widget(table, area);
    }

    fn draw_plan_modal(&self, frame: &mut Frame<'_>, area: Rect) {
        frame.render_widget(
            modal_block(" Plan ")
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

        let titles = PlanTab::all()
            .iter()
            .map(|tab| Line::from(Span::styled(tab.label(), value_style())))
            .collect::<Vec<_>>();
        let tabs = Tabs::new(titles)
            .select(self.plan_tab.index())
            .divider(Span::styled("  ", muted()))
            .highlight_style(prompt_style().add_modifier(Modifier::BOLD));
        frame.render_widget(tabs, sections[0]);

        let lines = plan_modal_lines(self);
        let line_count = lines.len();
        let max_scroll = line_count.saturating_sub(sections[1].height as usize);
        let scroll = self.plan_scroll.min(max_scroll);
        let body = Paragraph::new(lines)
            .style(Style::default().bg(surface()).fg(text()))
            .scroll((scroll.min(u16::MAX as usize) as u16, 0))
            .wrap(Wrap { trim: false });
        frame.render_widget(body, sections[1]);

        if max_scroll > 0 {
            let mut state = ScrollbarState::new(line_count)
                .position(scroll)
                .viewport_content_length(sections[1].height as usize);
            frame.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .thumb_symbol("█")
                    .track_symbol(Some("│")),
                sections[1],
                &mut state,
            );
        }

        let footer = Paragraph::new(Line::from(vec![
            Span::styled("h/l tab", prompt_style()),
            Span::styled("  ", muted()),
            Span::styled("j/k scroll", prompt_style()),
            Span::styled("  ", muted()),
            Span::styled("esc", prompt_style()),
            Span::styled(" close", muted()),
        ]))
        .alignment(Alignment::Right)
        .style(Style::default().bg(surface()));
        frame.render_widget(footer, sections[2]);
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

    fn recap_text(&self) -> String {
        let messages = self
            .transcript
            .iter()
            .filter(|item| matches!(item, TranscriptItem::Message(_)))
            .count();
        let tools = self
            .transcript
            .iter()
            .filter(|item| matches!(item, TranscriptItem::Tool(_)))
            .count();
        let running_jobs = self
            .background_jobs
            .values()
            .filter(|job| job.state == ToolRunState::Running)
            .count();
        format!(
            "※ recap\nworkspace: {}\nmessages: {messages}\ntool calls: {tools}\nworkflows: {} total · {} active\nbackground jobs: {} total · {running_jobs} running\nstatus: {}",
            self.cwd_display,
            self.workflows.len(),
            self.workflow_events.len(),
            self.background_jobs.len(),
            self.status_line,
        )
    }

    fn run_demo(&mut self) {
        self.transcript.push(TranscriptItem::Message(ChatMessage::system("Demo: Medusa can read/search/patch/run tools, show animated tool rows, and manage background jobs. Try `/exec --bg sleep 3 && echo done`, then `/jobs`.".to_string())));
        self.touch_transcript();
        self.push_tool_start("fs.list".to_string(), ".".to_string());
        self.push_tool_result(
            "fs.list",
            "root: .\n  Cargo.toml\n  crates/\n  README.md".to_string(),
        );
        self.push_tool_start(
            "terminal.exec".to_string(),
            "$ cargo check --dry-run".to_string(),
        );
        self.push_tool_result(
            "terminal.exec",
            "exit: 0\nstdout:\ncheck preview ok".to_string(),
        );
        self.status_line = "demo inserted".to_string();
        self.toast("Demo inserted", ToastKind::Success);
    }

    fn start_exec_command(&mut self, command: &str, background: bool) {
        self.push_tool_start("terminal.exec".to_string(), format!("$ {command}"));
        let request = TerminalExecRequest {
            command: command.to_string(),
            cwd: None,
            background,
        };
        match self
            .tools
            .clone()
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
        name: "/activity",
        args: "",
        category: "view",
        description: "Show model, tool, and session activity",
    },
    SlashCommand {
        name: "/plan",
        args: "",
        category: "view",
        description: "Show current checklist, blockers, and evidence",
    },
    SlashCommand {
        name: "/images",
        args: "",
        category: "view",
        description: "Preview attached images",
    },
    SlashCommand {
        name: "/workflows",
        args: "",
        category: "view",
        description: "Show workflow runs and subagent progress",
    },
    SlashCommand {
        name: "/workflow",
        args: "<task>",
        category: "agent",
        description: "Run a larger task through workflow subagents",
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
        name: "/clear",
        args: "",
        category: "session",
        description: "Clear the current transcript",
    },
    SlashCommand {
        name: "/themes",
        args: "",
        category: "theme",
        description: "Browse available UI themes",
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
        name: "/recap",
        args: "",
        category: "session",
        description: "Insert a compact session recap",
    },
    SlashCommand {
        name: "/demo",
        args: "",
        category: "system",
        description: "Insert a safe Medusa feature demo",
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
    SlashCommand {
        name: "/status",
        args: "<text>",
        category: "tools",
        description: "Update the status line",
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
];

fn slash_match_score(command: &SlashCommand, query: &str) -> Option<u8> {
    if query.is_empty() {
        return Some(10);
    }

    let name = command.name.trim_start_matches('/').to_ascii_lowercase();
    let category = command.category.to_ascii_lowercase();
    let args = command.args.to_ascii_lowercase();
    let description = command.description.to_ascii_lowercase();

    if name == query {
        Some(0)
    } else if name.starts_with(query) {
        Some(1)
    } else if name.contains(query) {
        Some(2)
    } else if category.starts_with(query) {
        Some(3)
    } else if category.contains(query) {
        Some(4)
    } else if description.contains(query) {
        Some(5)
    } else if args.contains(query) {
        Some(6)
    } else {
        None
    }
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
        "file.search    Search text in workspace",
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

fn append_plan_rows(rows: &mut Vec<TranscriptRow>, plan: &PlanView) {
    let progress = plan_progress(plan);
    let mut header = vec![
        Span::styled("plan", tool_label_style().add_modifier(Modifier::BOLD)),
        Span::styled(" · ", muted()),
        Span::styled(format!("{} steps", plan.items.len()), value_style()),
        Span::styled(" · ", muted()),
        Span::styled(format!("{} done", progress.done), success_style()),
    ];
    if progress.active > 0 {
        header.extend([
            Span::styled(" · ", muted()),
            Span::styled("active", prompt_style()),
        ]);
    }
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
    rows.push(TranscriptRow::text(Line::from(header)));

    let visible_count = if plan.expanded {
        plan.items.len()
    } else {
        plan.items.len().min(5)
    };
    for (index, item) in plan.items.iter().take(visible_count).enumerate() {
        rows.push(TranscriptRow::text(plan_item_line(
            item,
            index + 1 == visible_count && visible_count == plan.items.len(),
        )));
    }
    if visible_count < plan.items.len() {
        rows.push(TranscriptRow::text(Line::from(vec![
            Span::styled("  └─ ", muted()),
            Span::styled(
                format!(
                    "{} more steps · /plan for details",
                    plan.items.len() - visible_count
                ),
                muted(),
            ),
        ])));
    }
}

fn plan_item_line(item: &PlanItemView, last: bool) -> Line<'static> {
    Line::from(vec![
        Span::styled(if last { "  └─ " } else { "  ├─ " }, muted()),
        plan_status_marker_span(item.status),
        Span::styled(" ", muted()),
        Span::styled(truncate(&item.text, 120), plan_status_style(item.status)),
    ])
}

fn plan_modal_lines(app: &App) -> Vec<Line<'static>> {
    match app.plan_tab {
        PlanTab::Plan => plan_current_lines(app.current_plan()),
        PlanTab::History => plan_history_lines(&app.plan_history()),
        PlanTab::Blockers => plan_blocker_lines(app.current_plan()),
        PlanTab::Evidence => plan_evidence_lines(app.current_plan()),
    }
}

fn plan_current_lines(plan: Option<&PlanView>) -> Vec<Line<'static>> {
    let Some(plan) = plan else {
        return vec![
            Line::from(Span::styled("No plan yet.", muted())),
            Line::from(""),
            Line::from(vec![
                Span::styled("Ask Medusa to tackle a multi-step task, or use ", muted()),
                Span::styled("plan.update", prompt_style()),
                Span::styled(" from the model loop.", muted()),
            ]),
        ];
    };

    let progress = plan_progress(plan);
    let mut lines = vec![
        Line::from(vec![
            Span::styled("current", muted().add_modifier(Modifier::BOLD)),
            Span::styled("  ", muted()),
            Span::styled(
                if plan.summary.is_empty() {
                    "Untitled plan".to_string()
                } else {
                    plan.summary.clone()
                },
                prompt_style(),
            ),
        ]),
        Line::from(vec![
            Span::styled(format!("{} steps", plan.items.len()), value_style()),
            Span::styled(" · ", muted()),
            Span::styled(format!("{} pending", progress.pending), muted()),
            Span::styled(" · ", muted()),
            Span::styled(format!("{} active", progress.active), prompt_style()),
            Span::styled(" · ", muted()),
            Span::styled(format!("{} done", progress.done), success_style()),
            Span::styled(" · ", muted()),
            Span::styled(format!("{} blocked", progress.blocked), error_style()),
        ]),
        Line::from(""),
    ];

    for (index, item) in plan.items.iter().enumerate() {
        lines.push(Line::from(vec![
            Span::styled(format!("{:>2}. ", index + 1), muted()),
            plan_status_marker_span(item.status),
            Span::styled(" ", muted()),
            Span::styled(
                plan_status_label(item.status),
                plan_status_style(item.status),
            ),
            Span::styled("  ", muted()),
            Span::styled(item.text.clone(), value_style()),
        ]));
    }
    lines
}

fn plan_history_lines(history: &[&PlanView]) -> Vec<Line<'static>> {
    if history.is_empty() {
        return vec![Line::from(Span::styled("No plan snapshots yet.", muted()))];
    }

    let mut lines = Vec::new();
    for (index, plan) in history.iter().rev().take(12).enumerate() {
        let progress = plan_progress(plan);
        lines.push(Line::from(vec![
            Span::styled(
                format!("#{} ", history.len().saturating_sub(index)),
                muted(),
            ),
            Span::styled(
                if plan.summary.is_empty() {
                    "Untitled plan".to_string()
                } else {
                    truncate(&plan.summary, 56)
                },
                prompt_style(),
            ),
            Span::styled("  ", muted()),
            Span::styled(
                format!(
                    "{} steps · {} done · {} blocked",
                    plan.items.len(),
                    progress.done,
                    progress.blocked
                ),
                muted(),
            ),
        ]));
    }
    lines
}

fn plan_blocker_lines(plan: Option<&PlanView>) -> Vec<Line<'static>> {
    let Some(plan) = plan else {
        return vec![Line::from(Span::styled("No plan yet.", muted()))];
    };
    let blocked = plan
        .items
        .iter()
        .filter(|item| item.status == PlanItemStatus::Blocked)
        .collect::<Vec<_>>();
    if blocked.is_empty() {
        return vec![Line::from(Span::styled(
            "No blocked steps.",
            success_style(),
        ))];
    }

    let mut lines = Vec::new();
    for item in blocked {
        lines.push(Line::from(vec![
            Span::styled("× ", error_style()),
            Span::styled(item.text.clone(), value_style()),
        ]));
        for evidence in &item.evidence {
            lines.push(Line::from(vec![
                Span::styled("  └─ ", muted()),
                Span::styled(evidence.clone(), muted()),
            ]));
        }
    }
    lines
}

fn plan_evidence_lines(plan: Option<&PlanView>) -> Vec<Line<'static>> {
    let Some(plan) = plan else {
        return vec![Line::from(Span::styled("No plan yet.", muted()))];
    };

    let mut lines = Vec::new();
    for item in &plan.items {
        if item.evidence.is_empty() {
            continue;
        }
        lines.push(Line::from(vec![
            plan_status_marker_span(item.status),
            Span::styled(" ", muted()),
            Span::styled(item.text.clone(), value_style()),
        ]));
        for evidence in &item.evidence {
            lines.push(Line::from(vec![
                Span::styled("  └─ ", muted()),
                Span::styled(evidence.clone(), muted()),
            ]));
        }
        lines.push(Line::from(""));
    }

    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "No evidence attached to the current plan yet.",
            muted(),
        )));
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

fn plan_status_label(status: PlanItemStatus) -> &'static str {
    match status {
        PlanItemStatus::Pending => "pending",
        PlanItemStatus::Active => "active",
        PlanItemStatus::Done => "done",
        PlanItemStatus::Blocked => "blocked",
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

fn append_decision_rows(rows: &mut Vec<TranscriptRow>, decision: &DecisionView) {
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

    for (index, question) in decision.questions.iter().take(3).enumerate() {
        let answered = decision_question_answered(decision, question);
        rows.push(TranscriptRow::text(Line::from(vec![
            Span::styled(if index == 2 { "  └─ " } else { "  ├─ " }, muted()),
            Span::styled(
                if answered { "✓ " } else { "? " },
                if answered {
                    success_style()
                } else {
                    prompt_style()
                },
            ),
            Span::styled(truncate(&question.prompt, 120), value_style()),
        ])));
        if let Some(answer) = decision.answers.get(&question.id) {
            rows.push(TranscriptRow::text(Line::from(vec![
                Span::styled("  │  ", muted()),
                Span::styled(truncate(answer, 120), success_style()),
            ])));
        }
    }
    if decision.questions.len() > 3 {
        rows.push(TranscriptRow::text(Line::from(vec![
            Span::styled("  └─ ", muted()),
            Span::styled(
                format!("{} more question(s)", decision.questions.len() - 3),
                muted(),
            ),
        ])));
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

fn planning_rail_lines(app: &App, width: u16, max_lines: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    lines.push(Line::from(vec![
        Span::styled("Planning", accent().add_modifier(Modifier::BOLD)),
        Span::styled("  ", muted()),
        Span::styled("ctrl+←/→ resize", muted()),
    ]));
    lines.push(Line::from(""));

    if let Some(plan) = app.current_plan() {
        append_planning_rail_plan(&mut lines, plan, width);
    } else {
        lines.push(Line::from(Span::styled("No plan yet.", muted())));
    }

    lines.push(Line::from(""));

    if let Some(decision) = app.current_decision() {
        append_planning_rail_decision(
            &mut lines,
            decision,
            width,
            app.selected_decision_question_index(),
        );
    } else {
        lines.push(Line::from(Span::styled("No decisions queued.", muted())));
    }

    if lines.len() > max_lines {
        let keep = max_lines.saturating_sub(1);
        lines.truncate(keep);
        lines.push(Line::from(Span::styled("…", muted())));
    }

    lines
}

fn append_planning_rail_plan(lines: &mut Vec<Line<'static>>, plan: &PlanView, width: u16) {
    let progress = plan_progress(plan);
    lines.push(Line::from(vec![
        Span::styled("Plan", tool_label_style()),
        Span::styled("  ", muted()),
        Span::styled(
            format!("{} / {}", progress.done, plan.items.len()),
            success_style(),
        ),
    ]));
    if !plan.summary.trim().is_empty() {
        lines.push(Line::from(Span::styled(
            truncate_for_width(&plan.summary, width),
            muted(),
        )));
    }

    for item in plan.items.iter().take(7) {
        lines.push(Line::from(vec![
            plan_status_marker_span(item.status),
            Span::styled(" ", muted()),
            Span::styled(
                truncate_for_width(&item.text, width.saturating_sub(3)),
                plan_status_style(item.status),
            ),
        ]));
    }
    if plan.items.len() > 7 {
        lines.push(Line::from(Span::styled(
            format!("+{} more", plan.items.len() - 7),
            muted(),
        )));
    }
}

fn append_planning_rail_decision(
    lines: &mut Vec<Line<'static>>,
    decision: &DecisionView,
    width: u16,
    selected_question: usize,
) {
    lines.push(Line::from(vec![
        Span::styled("Decisions", prompt_style()),
        Span::styled("  ", muted()),
        Span::styled(
            if decision.answered {
                "answered"
            } else {
                "waiting"
            },
            if decision.answered {
                success_style()
            } else {
                prompt_style()
            },
        ),
    ]));
    if !decision.title.trim().is_empty() {
        lines.push(Line::from(Span::styled(
            truncate_for_width(&decision.title, width),
            value_style(),
        )));
    }
    if !decision.reason.trim().is_empty() {
        lines.push(Line::from(Span::styled(
            truncate_for_width(&decision.reason, width),
            muted(),
        )));
    }

    for (index, question) in decision.questions.iter().enumerate() {
        let selected = !decision.answered && index == selected_question;
        let answered = decision_question_answered(decision, question);
        let marker = if selected {
            "›"
        } else if answered {
            "✓"
        } else {
            "·"
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!("{marker} {}. ", index + 1),
                if selected { prompt_style() } else { muted() },
            ),
            Span::styled(
                truncate_for_width(&question.prompt, width.saturating_sub(6)),
                if selected {
                    prompt_style()
                } else {
                    value_style()
                },
            ),
        ]));
        if question.kind == DecisionQuestionKind::Choice {
            for option in question.options.iter().take(4) {
                let recommended = question.recommended.as_deref() == Some(option.as_str());
                let picked = decision.answers.get(&question.id) == Some(option);
                lines.push(Line::from(vec![
                    Span::styled("   ", muted()),
                    Span::styled(
                        if picked {
                            "● "
                        } else if recommended {
                            "◦ "
                        } else {
                            "  "
                        },
                        if picked {
                            success_style()
                        } else {
                            prompt_style()
                        },
                    ),
                    Span::styled(
                        truncate_for_width(option, width.saturating_sub(5)),
                        if picked {
                            success_style()
                        } else if selected || recommended {
                            prompt_style()
                        } else {
                            muted()
                        },
                    ),
                ]));
            }
        } else {
            let answer = decision.answers.get(&question.id);
            lines.push(Line::from(vec![
                Span::styled("   ", muted()),
                Span::styled(
                    answer
                        .map(|answer| truncate_for_width(answer, width.saturating_sub(5)))
                        .unwrap_or_else(|| "type in composer".to_string()),
                    if answer.is_some() {
                        success_style()
                    } else {
                        muted()
                    },
                ),
            ]));
        }
    }

    if !decision.assumptions.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("Assumptions", muted())));
        for assumption in decision.assumptions.iter().take(3) {
            lines.push(Line::from(vec![
                Span::styled("· ", muted()),
                Span::styled(
                    truncate_for_width(assumption, width.saturating_sub(2)),
                    muted(),
                ),
            ]));
        }
    }

    if let Some(answer) = &decision.answer {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("Answer  ", success_style()),
            Span::styled(
                truncate_for_width(answer, width.saturating_sub(8)),
                value_style(),
            ),
        ]));
    } else {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("Controls", prompt_style()),
            Span::styled(" · j/k question · h/l option · enter", muted()),
        ]));
    }
}

fn truncate_for_width(value: &str, width: u16) -> String {
    truncate(value, width.saturating_sub(1).max(8) as usize)
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
            let frame = animation::ThrobberKind::ToolPulse.frame(animation_tick);
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
            TranscriptItem::Plan(plan) => {
                if !rows.is_empty() {
                    rows.push(TranscriptRow::text(Line::from("")));
                }
                append_plan_rows(&mut rows, plan);
                index += 1;
            }
            TranscriptItem::Decision(decision) => {
                if !rows.is_empty() {
                    rows.push(TranscriptRow::text(Line::from("")));
                }
                append_decision_rows(&mut rows, decision);
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

fn launch_rows() -> Vec<TranscriptRow> {
    [
        Line::from(""),
        Line::from(vec![Span::styled(
            "  __  __ _____ ____  _   _ ____    _    ",
            accent().add_modifier(Modifier::BOLD),
        )]),
        Line::from(vec![Span::styled(
            " |  \\/  | ____|  _ \\| | | / ___|  / \\   ",
            accent().add_modifier(Modifier::BOLD),
        )]),
        Line::from(vec![Span::styled(
            " | |\\/| |  _| | | | | | | \\___ \\ / _ \\  ",
            accent().add_modifier(Modifier::BOLD),
        )]),
        Line::from(vec![Span::styled(
            " | |  | | |___| |_| | |_| |___) / ___ \\ ",
            accent().add_modifier(Modifier::BOLD),
        )]),
        Line::from(vec![Span::styled(
            " |_|  |_|_____|____/ \\___/|____/_/   \\_\\",
            accent().add_modifier(Modifier::BOLD),
        )]),
        Line::from(""),
        Line::from(vec![
            Span::styled("  terminal-native coding harness", value_style()),
            Span::styled("  ·  ", muted()),
            Span::styled("read", prompt_style()),
            Span::styled(" / ", muted()),
            Span::styled("edit", prompt_style()),
            Span::styled(" / ", muted()),
            Span::styled("patch", prompt_style()),
            Span::styled(" / ", muted()),
            Span::styled("test", prompt_style()),
            Span::styled(" / ", muted()),
            Span::styled("review", prompt_style()),
        ]),
        Line::from(vec![
            Span::styled("  type a task below", muted()),
            Span::styled("  ·  ", muted()),
            Span::styled("ctrl+p", prompt_style()),
            Span::styled(" palette  ", muted()),
            Span::styled("/settings", prompt_style()),
            Span::styled(" settings  ", muted()),
            Span::styled("ctrl+i", prompt_style()),
            Span::styled(" image", muted()),
        ]),
        Line::from(vec![
            Span::styled("  ", muted()),
            Span::styled("● ", success_style()),
            Span::styled("ready", muted()),
            Span::styled("    ", muted()),
            Span::styled("● ", tool_label_style()),
            Span::styled("tools when needed", muted()),
            Span::styled("    ", muted()),
            Span::styled("● ", prompt_style()),
            Span::styled("workflows for larger changes", muted()),
        ]),
    ]
    .into_iter()
    .map(TranscriptRow::text)
    .collect()
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

fn markdown_content_lines(content: &str, role: ChatRole) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut in_code_block = false;

    for raw_line in content.lines() {
        let line = raw_line.trim_end();
        let trimmed = line.trim_start();

        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_code_block = !in_code_block;
            continue;
        }

        if in_code_block {
            lines.push(Line::from(vec![
                Span::styled("  │ ", code_border_style()),
                Span::styled(line.to_string(), code_block_style()),
            ]));
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

fn append_tool_group_lines(
    lines: &mut Vec<Line<'static>>,
    transcript: &[TranscriptItem],
    start: usize,
    end: usize,
    selected_tool: Option<usize>,
    context: RenderContext,
) {
    let runs = transcript[start..end]
        .iter()
        .filter_map(|item| match item {
            TranscriptItem::Tool(run) => Some(run),
            _ => None,
        })
        .collect::<Vec<_>>();

    if runs.is_empty() {
        return;
    }

    let status = tool_group_status(&runs);
    let selected = selected_tool.is_some_and(|index| (start..end).contains(&index));
    let expanded = selected
        && transcript[start..end]
            .iter()
            .any(|item| matches!(item, TranscriptItem::Tool(run) if run.expanded));
    let row_style = if selected {
        activity_selected_style()
    } else {
        Style::default()
    };
    let selected_style = |style: Style| style.patch(row_style);
    let call_label = if runs.len() == 1 { "call" } else { "calls" };
    let disclosure = if selected {
        if expanded { "▾ " } else { "▸ " }
    } else {
        "  "
    };
    let breakdown = tool_group_breakdown(&runs);
    if status == ToolRunState::Running {
        let mut row = vec![Span::styled(disclosure, selected_style(muted()))];
        row.push(tool_running_marker_span(status, context.animation_tick));
        row.push(Span::raw(" "));
        row.push(Span::styled(
            "tools",
            selected_style(tool_group_label_style()),
        ));
        row.push(Span::styled(
            format!("  {} {call_label}  ", runs.len()),
            selected_style(tool_group_meta_style()),
        ));
        row.push(Span::styled(
            breakdown,
            selected_style(message_style(ChatRole::Tool)),
        ));
        lines.push(Line::from(row));
    } else if status == ToolRunState::Failed {
        let mut row = vec![
            Span::styled(disclosure, selected_style(muted())),
            Span::styled("tools", selected_style(tool_group_label_style())),
            Span::styled(
                format!("  {} {call_label}  ", runs.len()),
                selected_style(tool_group_meta_style()),
            ),
            Span::styled(breakdown, selected_style(message_style(ChatRole::Tool))),
        ];
        row.push(Span::styled("  ", selected_style(tool_group_meta_style())));
        row.push(Span::styled("failed", selected_style(error_style())));
        lines.push(Line::from(row));
    } else {
        lines.push(Line::from(vec![
            Span::styled(disclosure, selected_style(muted())),
            Span::styled("tools", selected_style(tool_group_label_style())),
            Span::styled(
                format!("  {} {call_label}  ", runs.len()),
                selected_style(tool_group_meta_style()),
            ),
            Span::styled(breakdown, selected_style(message_style(ChatRole::Tool))),
        ]));
    }

    if expanded {
        append_tool_group_detail_lines(lines, &runs, context);
    } else {
        append_tool_group_subtree_lines(lines, &runs, context);
        if status == ToolRunState::Failed {
            append_tool_failure_preview(lines, &runs);
        }
    }
}

fn append_tool_group_subtree_lines(
    lines: &mut Vec<Line<'static>>,
    runs: &[&ToolRun],
    context: RenderContext,
) {
    let items = tool_group_subtree_items(runs);
    let hidden = items.len().saturating_sub(4);
    let visible_count = items.len().min(4);
    for (index, item) in items.iter().take(visible_count).enumerate() {
        append_tool_group_subtree_item_line(
            lines,
            item,
            index + 1 == visible_count && hidden == 0,
            context,
        );
    }

    if hidden > 0 {
        lines.push(Line::from(vec![
            Span::styled("    └─ ", separator_style()),
            Span::styled(
                format!(
                    "… {hidden} more categor{} hidden",
                    if hidden == 1 { "y" } else { "ies" }
                ),
                muted(),
            ),
        ]));
    }
}

fn append_tool_group_subtree_item_line(
    lines: &mut Vec<Line<'static>>,
    item: &ToolSubtreeItem,
    is_last: bool,
    context: RenderContext,
) {
    let branch = if is_last { "└─" } else { "├─" };
    let mut row = vec![
        Span::styled("    ", tool_group_meta_style()),
        Span::styled(branch, separator_style()),
        Span::raw(" "),
        tool_running_marker_span(item.state, context.animation_tick),
        Span::raw(" "),
        Span::styled(item.summary(), tool_label_style()),
    ];
    let detail = item.detail();
    if !detail.is_empty() {
        row.push(Span::styled(" · ", muted()));
        row.push(Span::styled(
            truncate(&detail, 120),
            message_style(ChatRole::Tool),
        ));
    }
    lines.push(Line::from(row));
}

#[cfg(test)]
fn append_compact_tool_call_lines(
    lines: &mut Vec<Line<'static>>,
    run: &ToolRun,
    context: RenderContext,
) {
    let card = tool_activity_card(run);
    let action_style = if run.state == ToolRunState::Running {
        tool_label_style().add_modifier(Modifier::BOLD)
    } else {
        tool_label_style()
    };
    lines.push(Line::from(vec![
        Span::styled("    ", tool_group_meta_style()),
        tool_running_marker_span(run.state, context.animation_tick),
        Span::raw(" "),
        Span::styled(card.action, action_style),
        Span::raw(" "),
        Span::styled(truncate(&card.target, 120), message_style(ChatRole::Tool)),
    ]));

    let mut result = vec![Span::styled("      ⎿ ", tool_output_border_style())];
    result.extend(tool_result_spans(run, context.animation_tick));
    lines.push(Line::from(result));
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

fn tool_running_text_style(animation_tick: u64) -> Style {
    let _ = animation_tick;
    muted()
}

fn tool_running_verb(animation_tick: u64) -> &'static str {
    let _ = animation_tick;
    "running"
}

fn tool_status_spans(state: ToolRunState, animation_tick: u64) -> Vec<Span<'static>> {
    match state {
        ToolRunState::Running => {
            vec![Span::styled(
                tool_running_verb(animation_tick),
                tool_running_text_style(animation_tick),
            )]
        }
        ToolRunState::Succeeded => vec![Span::styled("done", success_style())],
        ToolRunState::Failed => vec![Span::styled("failed", error_style())],
    }
}

#[cfg(test)]
fn tool_result_spans(run: &ToolRun, animation_tick: u64) -> Vec<Span<'static>> {
    match run.state {
        ToolRunState::Running => {
            vec![Span::styled(
                tool_running_verb(animation_tick),
                tool_running_text_style(animation_tick),
            )]
        }
        ToolRunState::Succeeded => vec![Span::styled(
            tool_result_line(run),
            tool_output_style(run.state),
        )],
        ToolRunState::Failed => vec![Span::styled(
            tool_result_line(run),
            tool_output_style(run.state),
        )],
    }
}

#[cfg(test)]
fn tool_result_line(run: &ToolRun) -> String {
    match run.state {
        ToolRunState::Running => "working…".to_string(),
        ToolRunState::Succeeded => {
            let preview = tool_result_preview(run).unwrap_or_else(|| "done".to_string());
            format!("done · {}", compact_one_line(&preview, 140))
        }
        ToolRunState::Failed => {
            let preview = tool_result_preview(run)
                .or_else(|| tool_failure_preview(&[run]).map(|(_, preview)| preview))
                .unwrap_or_else(|| "failed".to_string());
            format!("failed · {}", compact_one_line(&preview, 140))
        }
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

fn append_tool_group_detail_lines(
    lines: &mut Vec<Line<'static>>,
    runs: &[&ToolRun],
    context: RenderContext,
) {
    let hidden = runs.len().saturating_sub(5);
    if hidden > 0 {
        lines.push(Line::from(vec![
            Span::styled("      ", muted()),
            Span::styled(format!("{hidden} earlier calls hidden"), muted()),
        ]));
    }

    let visible_start = runs.len().saturating_sub(5);
    for run in &runs[visible_start..] {
        append_tool_run_lines(
            lines,
            run,
            runs.len() == 1 || run.state == ToolRunState::Failed,
            context,
        );
    }
}

fn append_tool_run_lines(
    lines: &mut Vec<Line<'static>>,
    run: &ToolRun,
    include_output: bool,
    context: RenderContext,
) {
    let card = tool_activity_card(run);
    let action_style = if run.state == ToolRunState::Running {
        tool_label_style().add_modifier(Modifier::BOLD)
    } else {
        tool_label_style()
    };

    let mut row = vec![
        Span::styled("      ", tool_group_meta_style()),
        tool_running_marker_span(run.state, context.animation_tick),
        Span::raw(" "),
        Span::styled(format!("{:<8}", card.action), action_style),
        Span::raw(" "),
        Span::styled(truncate(&card.target, 110), message_style(ChatRole::Tool)),
        Span::raw("  "),
    ];
    row.extend(tool_status_spans(run.state, context.animation_tick));
    lines.push(Line::from(row));

    if include_output {
        append_tool_output_lines(lines, run);
    }
}

fn tool_state_label(state: ToolRunState) -> &'static str {
    match state {
        ToolRunState::Running => "working",
        ToolRunState::Succeeded => "done",
        ToolRunState::Failed => "failed",
    }
}

fn tool_state_style(state: ToolRunState) -> Style {
    match state {
        ToolRunState::Running => muted(),
        ToolRunState::Succeeded => success_style(),
        ToolRunState::Failed => error_style(),
    }
}

fn tool_output_style(state: ToolRunState) -> Style {
    match state {
        ToolRunState::Failed => error_preview_style(),
        _ => message_style(ChatRole::Tool),
    }
}

fn append_tool_output_lines(lines: &mut Vec<Line<'static>>, run: &ToolRun) {
    lines.push(Line::from(vec![
        Span::styled("      hint ", muted()),
        Span::styled(
            "enter expand/collapse · y copy soon · failures auto-expand",
            muted(),
        ),
    ]));
    let detail = run.detail.trim();
    if detail.is_empty() || detail == "done" {
        lines.push(Line::from(vec![
            Span::styled("      output ", muted()),
            Span::styled("no details", muted()),
        ]));
        return;
    }

    for line in detail
        .lines()
        .filter(|line| !line.trim().is_empty())
        .take(10)
    {
        lines.push(Line::from(vec![
            Span::styled("      │ ", tool_output_border_style()),
            Span::styled(truncate(line.trim(), 180), tool_output_style(run.state)),
        ]));
    }
}

fn append_tool_failure_preview(lines: &mut Vec<Line<'static>>, runs: &[&ToolRun]) {
    let Some((name, preview)) = tool_failure_preview(runs) else {
        return;
    };

    lines.push(Line::from(vec![
        Span::styled("      ", muted()),
        Span::styled(format!("{name} "), tool_label_style()),
        Span::styled(truncate(&preview, 170), error_preview_style()),
    ]));
}

fn tool_failure_preview(runs: &[&ToolRun]) -> Option<(String, String)> {
    runs.iter()
        .rev()
        .find(|run| run.state == ToolRunState::Failed)
        .and_then(|run| {
            let preview = run
                .detail
                .lines()
                .map(str::trim)
                .find(|line| {
                    !line.is_empty()
                        && *line != "stdout:"
                        && *line != "stderr:"
                        && *line != "stdout: <empty>"
                        && !line.starts_with("exit: ")
                })
                .or_else(|| {
                    run.detail
                        .lines()
                        .map(str::trim)
                        .find(|line| !line.is_empty())
                })?;

            Some((
                tool_display_name(&run.name).to_string(),
                preview.to_string(),
            ))
        })
}

fn tool_group_status(runs: &[&ToolRun]) -> ToolRunState {
    if runs.iter().any(|run| run.state == ToolRunState::Running) {
        ToolRunState::Running
    } else if runs.iter().any(|run| run.state == ToolRunState::Failed) {
        ToolRunState::Failed
    } else {
        ToolRunState::Succeeded
    }
}

fn tool_group_breakdown(runs: &[&ToolRun]) -> String {
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for run in runs {
        let name = tool_display_name(&run.name);
        if let Some((_, count)) = counts.iter_mut().find(|(existing, _)| *existing == name) {
            *count += 1;
        } else {
            counts.push((name, 1));
        }
    }

    counts
        .into_iter()
        .take(4)
        .map(|(name, count)| {
            if count == 1 {
                name.to_string()
            } else {
                format!("{name} x{count}")
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[derive(Debug, Clone)]
struct ToolSubtreeItem {
    key: &'static str,
    noun: &'static str,
    verb: &'static str,
    count: usize,
    state: ToolRunState,
    sample: Option<String>,
}

impl ToolSubtreeItem {
    fn summary(&self) -> String {
        format!(
            "{} {} {}",
            self.verb,
            self.count,
            pluralize(self.noun, self.count)
        )
    }

    fn detail(&self) -> String {
        self.sample.clone().unwrap_or_default()
    }
}

fn tool_group_subtree_items(runs: &[&ToolRun]) -> Vec<ToolSubtreeItem> {
    let mut items: Vec<ToolSubtreeItem> = Vec::new();
    for run in runs {
        let bucket = tool_subtree_bucket(run);
        if let Some(index) = items.iter().position(|item| item.key == bucket.key) {
            let item = &mut items[index];
            item.count += 1;
            item.state = merge_tool_state(item.state, run.state);
            if item.sample.is_none() {
                item.sample = tool_subtree_sample(run);
            }
            continue;
        }

        items.push(ToolSubtreeItem {
            key: bucket.key,
            noun: bucket.noun,
            verb: bucket.verb,
            count: 1,
            state: run.state,
            sample: tool_subtree_sample(run),
        });
    }
    items
}

#[derive(Debug, Clone, Copy)]
struct ToolSubtreeBucket {
    key: &'static str,
    noun: &'static str,
    verb: &'static str,
}

fn tool_subtree_bucket(run: &ToolRun) -> ToolSubtreeBucket {
    match run.name.as_str() {
        "file.read" => ToolSubtreeBucket {
            key: "read",
            noun: "file",
            verb: "read",
        },
        "file.search" => ToolSubtreeBucket {
            key: "search",
            noun: "query",
            verb: "searched",
        },
        "fs.list" => ToolSubtreeBucket {
            key: "list",
            noun: "path",
            verb: "listed",
        },
        "file.edit" | "file.patch" => ToolSubtreeBucket {
            key: "edit",
            noun: "file",
            verb: "edited",
        },
        "task.update" => ToolSubtreeBucket {
            key: "status",
            noun: "update",
            verb: "posted",
        },
        "terminal.exec" => terminal_subtree_bucket(run),
        _ => ToolSubtreeBucket {
            key: "tool",
            noun: "call",
            verb: "ran",
        },
    }
}

fn terminal_subtree_bucket(run: &ToolRun) -> ToolSubtreeBucket {
    match classify_command_action(&command_from_summary(&run.summary)) {
        "Read" => ToolSubtreeBucket {
            key: "read",
            noun: "file",
            verb: "read",
        },
        "Search" => ToolSubtreeBucket {
            key: "search",
            noun: "query",
            verb: "searched",
        },
        "List" => ToolSubtreeBucket {
            key: "list",
            noun: "path",
            verb: "listed",
        },
        "Test" => ToolSubtreeBucket {
            key: "test",
            noun: "command",
            verb: "tested",
        },
        "Build" => ToolSubtreeBucket {
            key: "build",
            noun: "command",
            verb: "built",
        },
        "Format" => ToolSubtreeBucket {
            key: "format",
            noun: "command",
            verb: "formatted",
        },
        "Git" => ToolSubtreeBucket {
            key: "git",
            noun: "command",
            verb: "ran git",
        },
        _ => ToolSubtreeBucket {
            key: "terminal",
            noun: "command",
            verb: "ran",
        },
    }
}

fn tool_subtree_sample(run: &ToolRun) -> Option<String> {
    let card = tool_activity_card(run);
    let target = compact_tool_target(&card.target, 70);
    if target.is_empty() {
        None
    } else {
        Some(target)
    }
}

fn merge_tool_state(left: ToolRunState, right: ToolRunState) -> ToolRunState {
    if matches!(left, ToolRunState::Failed) || matches!(right, ToolRunState::Failed) {
        ToolRunState::Failed
    } else if matches!(left, ToolRunState::Running) || matches!(right, ToolRunState::Running) {
        ToolRunState::Running
    } else {
        ToolRunState::Succeeded
    }
}

fn pluralize(noun: &str, count: usize) -> String {
    if count == 1 {
        noun.to_string()
    } else if noun.ends_with('y') {
        format!("{}ies", noun.trim_end_matches('y'))
    } else {
        format!("{noun}s")
    }
}

fn tool_display_name(name: &str) -> &str {
    match name {
        "file.read" => "read",
        "file.search" => "search",
        "fs.list" => "list",
        "terminal.exec" => "terminal",
        "file.edit" => "edit",
        "file.patch" => "patch",
        "task.update" => "status",
        other => other,
    }
}

#[derive(Debug, Clone)]
struct ToolActivityCard {
    action: String,
    target: String,
    preview: Option<String>,
}

fn tool_activity_card(run: &ToolRun) -> ToolActivityCard {
    match run.name.as_str() {
        "terminal.exec" => terminal_activity_card(run),
        "file.edit" => patch_activity_card(run),
        "file.patch" => patch_activity_card(run),
        _ => ToolActivityCard {
            action: tool_display_name(&run.name).to_string(),
            target: tool_summary(&run.summary),
            preview: tool_result_preview(run),
        },
    }
}

fn meaningful_tool_output_lines(run: &ToolRun) -> Vec<String> {
    run.detail
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && *line != "done")
        .map(ToString::to_string)
        .collect()
}

fn output_count_label(count: usize) -> String {
    match count {
        0 => "no details".to_string(),
        1 => "1 line".to_string(),
        count => format!("{count} lines"),
    }
}

fn tool_activity_output_lines(run: &ToolRun, width: u16) -> Vec<Line<'static>> {
    let output = meaningful_tool_output_lines(run);
    if output.is_empty() {
        return vec![Line::from(Span::styled("no output details", muted()))];
    }

    let max_width = width.saturating_sub(2).max(12) as usize;
    output
        .into_iter()
        .map(|line| {
            let style = if line.starts_with("stderr:")
                || line.starts_with("error:")
                || line.starts_with("failed")
            {
                error_preview_style()
            } else if line.starts_with("stdout:") || line.starts_with("exit:") {
                muted()
            } else {
                tool_output_style(run.state)
            };
            Line::from(vec![
                Span::styled("│ ", tool_output_border_style()),
                Span::styled(truncate(&line, max_width), style),
            ])
        })
        .collect()
}

fn tool_activity_timeline_lines(run: &ToolRun) -> Vec<Line<'static>> {
    let card = tool_activity_card(run);
    let elapsed = run.started_at.elapsed().as_secs();
    let result =
        tool_result_preview(run).unwrap_or_else(|| tool_state_label(run.state).to_string());
    vec![
        Line::from(vec![
            Span::styled("● ", tool_label_style()),
            Span::styled("started", value_style()),
            Span::styled(" · ", muted()),
            Span::styled(card.action, tool_label_style()),
            Span::styled(" · ", muted()),
            Span::styled(
                compact_tool_target(&card.target, 90),
                message_style(ChatRole::Tool),
            ),
        ]),
        Line::from(vec![
            Span::styled("│ ", separator_style()),
            Span::styled("elapsed ", muted()),
            Span::styled(format!("{elapsed}s"), value_style()),
        ]),
        Line::from(vec![
            Span::styled("└ ", tool_state_style(run.state)),
            Span::styled(tool_state_label(run.state), tool_state_style(run.state)),
            Span::styled(" · ", muted()),
            Span::styled(compact_one_line(&result, 100), tool_output_style(run.state)),
        ]),
    ]
}

fn terminal_activity_card(run: &ToolRun) -> ToolActivityCard {
    let command = command_from_summary(&run.summary);
    ToolActivityCard {
        action: classify_command_action(&command).to_string(),
        target: if command.is_empty() {
            tool_summary(&run.summary)
        } else {
            command
        },
        preview: tool_result_preview(run),
    }
}

fn patch_activity_card(run: &ToolRun) -> ToolActivityCard {
    ToolActivityCard {
        action: match run.state {
            ToolRunState::Running | ToolRunState::Succeeded => "Edit".to_string(),
            ToolRunState::Failed => "Edit failed".to_string(),
        },
        target: patch_detail(run),
        preview: tool_result_preview(run),
    }
}

fn tool_activity_header_line(phase: ToolActivityPhase, runs: &[ToolRun]) -> Line<'static> {
    let failed = runs
        .iter()
        .filter(|run| run.state == ToolRunState::Failed)
        .count();
    let running = runs
        .iter()
        .filter(|run| run.state == ToolRunState::Running)
        .count();
    let edited = runs.iter().any(|run| run.name == "file.patch");

    let (label, style) = if failed > 0 {
        (
            format!("Failed · {failed}/{} tools", runs.len()),
            error_style(),
        )
    } else if running > 0 {
        ("Working…".to_string(), tool_label_style())
    } else {
        let noun = if edited { "changes" } else { "checks" };
        match phase {
            ToolActivityPhase::LastTurn | ToolActivityPhase::CurrentTurn => {
                (format!("Done · {} {noun}", runs.len()), success_style())
            }
            ToolActivityPhase::Idle => ("Idle".to_string(), muted()),
        }
    };

    Line::from(vec![
        Span::styled("● ", style),
        Span::styled(label, style.add_modifier(Modifier::BOLD)),
    ])
}

fn command_from_summary(summary: &str) -> String {
    let summary = tool_summary(summary);
    summary
        .strip_prefix('$')
        .map(str::trim)
        .unwrap_or(summary.trim())
        .to_string()
}

fn classify_command_action(command: &str) -> &'static str {
    let command = command.trim_start();
    let first = command.split_whitespace().next().unwrap_or("");
    if command.is_empty() {
        return "Bash";
    }
    if command.contains(" test")
        || command.starts_with("cargo test")
        || command.starts_with("npm test")
        || command.starts_with("pnpm test")
        || command.starts_with("yarn test")
        || command.starts_with("pytest")
        || command.starts_with("go test")
    {
        "Test"
    } else if command.contains(" build")
        || command.starts_with("cargo build")
        || command.starts_with("npm run build")
        || command.starts_with("pnpm build")
        || command.starts_with("yarn build")
        || command.starts_with("go build")
    {
        "Build"
    } else if command.contains(" fmt")
        || command.starts_with("cargo fmt")
        || command.starts_with("rustfmt")
        || command.starts_with("prettier")
        || command.starts_with("npm run format")
    {
        "Format"
    } else if matches!(first, "rg" | "grep" | "ack" | "ag") {
        "Search"
    } else if matches!(first, "cat" | "sed" | "head" | "tail" | "bat") {
        "Read"
    } else if matches!(first, "ls" | "find" | "fd" | "tree" | "pwd") {
        "List"
    } else if matches!(first, "git") {
        "Git"
    } else {
        "Bash"
    }
}

fn patch_detail(run: &ToolRun) -> String {
    if run.state == ToolRunState::Succeeded {
        let files = run
            .detail
            .lines()
            .filter(|line| !line.trim().is_empty() && *line != "done")
            .collect::<Vec<_>>();
        return match files.len() {
            0 => "Patch applied".to_string(),
            1 => files[0].trim().to_string(),
            count => format!("{count} files changed"),
        };
    }

    let summary = tool_summary(&run.summary);
    if summary == "tool call" {
        "patch".to_string()
    } else {
        summary
    }
}

fn tool_result_preview(run: &ToolRun) -> Option<String> {
    match run.state {
        ToolRunState::Running => None,
        ToolRunState::Succeeded => success_result_preview(run),
        ToolRunState::Failed => failure_result_preview(run),
    }
}

fn success_result_preview(run: &ToolRun) -> Option<String> {
    if run.name == "file.patch" {
        return None;
    }
    if run.name == "terminal.exec" && command_success_is_noise(&command_from_summary(&run.summary))
    {
        return None;
    }
    let detail = run.detail.trim();
    if detail.is_empty() || detail == "done" {
        return None;
    }
    detail
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && *line != "exit: 0")
        .map(|line| compact_one_line(line, 90))
}

fn command_success_is_noise(command: &str) -> bool {
    let first = command.split_whitespace().next().unwrap_or("");
    matches!(
        first,
        "rg" | "grep"
            | "ack"
            | "ag"
            | "cat"
            | "sed"
            | "head"
            | "tail"
            | "bat"
            | "ls"
            | "find"
            | "fd"
            | "tree"
            | "pwd"
            | "git"
    )
}

fn failure_result_preview(run: &ToolRun) -> Option<String> {
    if let Some((_, preview)) = tool_failure_preview(&[run]) {
        Some(preview)
    } else {
        Some("failed".to_string())
    }
}

fn tool_activity_item(
    run: &ToolRun,
    selected: bool,
    animation_tick: u64,
    width: u16,
) -> ListItem<'static> {
    let card = tool_activity_card(run);
    let (marker, marker_style) = match run.state {
        ToolRunState::Running => {
            let frame = animation::ThrobberKind::BrailleOrbit.frame(animation_tick);
            (frame.symbol.to_string(), tool_label_style())
        }
        ToolRunState::Succeeded => ("✓".to_string(), success_style()),
        ToolRunState::Failed => ("✕".to_string(), error_style()),
    };
    let action_style = match run.state {
        ToolRunState::Running => tool_label_style().add_modifier(Modifier::BOLD),
        ToolRunState::Succeeded => message_style(ChatRole::Assistant).add_modifier(Modifier::BOLD),
        ToolRunState::Failed => error_style().add_modifier(Modifier::BOLD),
    };
    let target_width = width.saturating_sub(15).max(8) as usize;
    let preview_width = width.saturating_sub(8).max(8) as usize;
    let target = compact_tool_target(&card.target, target_width);
    let branch = if selected { "╞" } else { "├" };

    let mut lines = vec![Line::from(vec![
        Span::styled(
            branch,
            if selected {
                accent()
            } else {
                separator_style()
            },
        ),
        Span::styled("─ ", separator_style()),
        Span::styled(marker, marker_style),
        Span::raw(" "),
        Span::styled(card.action, action_style),
        Span::styled("(", muted()),
        Span::styled(target, message_style(ChatRole::Tool)),
        Span::styled(")", muted()),
    ])];

    if let Some(result) = card.preview.filter(|preview| !preview.trim().is_empty()) {
        let result_style = if run.state == ToolRunState::Failed {
            error_preview_style()
        } else {
            muted()
        };
        lines.push(Line::from(vec![
            Span::styled("│  ⎿ ", separator_style()),
            Span::styled(truncate(&result, preview_width), result_style),
        ]));
    }
    lines.push(Line::from(Span::styled("│", separator_style())));

    ListItem::new(lines).style(Style::default().bg(surface()))
}

fn tool_activity_list_height(runs: &[ToolRun], width: u16) -> u16 {
    let mut height = 2;
    for run in runs {
        let card = tool_activity_card(run);
        height += 1;
        if card.preview.is_some_and(|result| !result.trim().is_empty()) {
            height += 1;
        }
    }
    height.min(width.saturating_add(height))
}

fn compact_tool_target(target: &str, max_chars: usize) -> String {
    let target = compact_one_line(target, max_chars);
    target
        .strip_prefix("$ ")
        .unwrap_or(&target)
        .trim()
        .to_string()
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

    if output.starts_with("exit: 0")
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
            .take(4)
            .collect::<Vec<_>>()
            .join("\n")
            .if_empty("done");
    }

    output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .take(4)
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

fn truncate(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let truncated = chars.by_ref().take(max_chars).collect::<String>();

    if chars.next().is_some() {
        format!("{truncated}...")
    } else {
        truncated
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

fn tool_output_border_style() -> Style {
    Style::default().fg(palette().tool)
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
    vec![
        Line::from(vec![
            Span::styled(selected.to_string(), prompt_style()),
            Span::styled("  ", muted()),
            Span::styled(
                if is_active { "active" } else { "ready to save" },
                if is_active { success_style() } else { muted() },
            ),
        ]),
        Line::from(""),
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
    ]
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

fn permission_context_text(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Open => {
            "Medusa permission mode: open. Normal workspace inspection, terminal commands, and file mutations are available subject to workspace boundaries."
        }
        PermissionMode::Guarded => {
            "Medusa permission mode: guarded. Workspace inspection is available. Terminal commands and file mutations are allowed unless blocked by Medusa's guarded permission policy. Mention guarded mode when a command or edit is blocked."
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
            _ => None,
        },
        _ => None,
    }
}

fn conversation_message_cost(message: &ConversationMessage) -> usize {
    message
        .content
        .chars()
        .count()
        .saturating_add(message.attachments.len().saturating_mul(2_000))
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

fn activity_scrollbar() -> Scrollbar<'static> {
    Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .thumb_symbol("█")
        .track_symbol(Some("│"))
        .thumb_style(accent())
        .track_style(separator_style())
}

fn panel_block(title: &'static str, focused: bool) -> Block<'static> {
    let border_style = if focused { accent() } else { separator_style() };
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(title)
        .border_style(border_style)
        .style(Style::default().bg(surface()).fg(text()))
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
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = env::temp_dir().join(format!("medusa-tui-test-{suffix}"));
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

        unsafe { env::set_var("MEDUSA_THEME", "nord") };
        assert_eq!(
            ThemeKind::from_workspace_settings(&workspace),
            ThemeKind::Nord
        );
        unsafe { env::remove_var("MEDUSA_THEME") };
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
    fn images_command_opens_preview_modal() {
        let mut app = app();
        app.pending_attachments.push(image_attachment("clipboard"));

        assert!(app.run_local_tool_command("/images"));

        assert_eq!(app.active_modal, Some(Modal::ImagePreview));
        assert_eq!(app.image_preview_index, 0);
        assert_eq!(app.status_line, "image preview 1/1");
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
        app.last_transcript_rows = vec![
            TranscriptRow::text(Line::from("before image")),
            TranscriptRow::image(Line::from("image placeholder"), attachment),
        ];
        app.last_transcript_rows
            .extend((1..CHAT_IMAGE_PREVIEW_HEIGHT).map(|_| TranscriptRow::text(Line::from(""))));

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
        assert!(messages[1].content.contains("older messages summarized"));
        assert!(messages[1].content.contains("old task 39"));
        assert!(
            !messages
                .iter()
                .skip(2)
                .any(|message| { message.role == "user" && message.content == "old task 0" })
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
        assert!(
            !messages
                .iter()
                .skip(2)
                .any(|message| { message.content.contains("I prefer concise answers") })
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
        assert!(matches.iter().any(|command| command.name == "/settings"));
    }

    #[test]
    fn slash_prefix_suggests_fork() {
        let mut app = app();

        app.input = "/fo".to_string();
        app.input_cursor = 3;

        let matches = app.slash_matches();
        assert!(matches.iter().any(|command| command.name == "/fork"));
    }

    #[test]
    fn slash_prefix_suggests_resume() {
        let mut app = app();

        app.input = "/re".to_string();
        app.input_cursor = 3;

        let matches = app.slash_matches();
        assert!(matches.iter().any(|command| command.name == "/resume"));
    }

    #[test]
    fn slash_prefix_suggests_tree() {
        let mut app = app();

        app.input = "/tr".to_string();
        app.input_cursor = 3;

        let matches = app.slash_matches();
        assert!(matches.iter().any(|command| command.name == "/tree"));
    }

    #[test]
    fn slash_prefix_suggests_skills() {
        let mut app = app();

        app.input = "/sk".to_string();
        app.input_cursor = 3;

        let matches = app.slash_matches();
        assert!(matches.iter().any(|command| command.name == "/skills"));
    }

    #[test]
    fn slash_prefix_suggests_workflow_commands() {
        let mut app = app();

        app.input = "/work".to_string();
        app.input_cursor = app.input_len();

        let matches = app.slash_matches();
        assert!(matches.iter().any(|command| command.name == "/workflow"));
        assert!(matches.iter().any(|command| command.name == "/workflows"));
    }

    #[test]
    fn slash_search_matches_description_and_category() {
        let mut app = app();

        app.input = "/browse".to_string();
        app.input_cursor = app.input_len();

        let matches = app.slash_matches();
        assert!(matches.iter().any(|command| command.name == "/themes"));

        app.input = "/session".to_string();
        app.input_cursor = app.input_len();
        let matches = app.slash_matches();
        assert!(matches.iter().any(|command| command.name == "/resume"));
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

        app.input = "/themes".to_string();
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
        assert!(first_span_fg_containing(&updated, "tools").is_some());
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
        assert_eq!(app.theme, ThemeKind::MaterialRose);
        assert_eq!(app.status_line, "theme: material-rose");
    }

    #[test]
    fn slash_theme_prefix_suggests_theme_names() {
        let mut app = app();
        app.input = "/theme mat".to_string();
        app.input_cursor = app.input_len();

        let names = app
            .slash_matches()
            .into_iter()
            .map(|command| command.name)
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
    fn status_command_updates_visible_state() {
        let mut app = app();

        app.input = "/status reading repo".to_string();
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(app.status_line, "reading repo");
        assert!(app.transcript.is_empty());
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
        app.workflow_events.push(receiver);

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
        app.workflow_events.push(finished_receiver);
        app.workflow_events.push(active_receiver);

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

        assert!(text.iter().any(|line| line.contains("____")));
        assert!(
            text.iter()
                .any(|line| line.contains("terminal-native coding harness"))
        );
        assert!(text.iter().any(|line| line.contains("/settings")));
        assert!(text.iter().any(|line| line.contains("ctrl+p")));
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
                .any(|line| line.contains("terminal-native coding harness"))
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
        for run in &mut app.activity_tools {
            run.started_at = run
                .started_at
                .checked_sub(MIN_TOOL_PULSE_VISIBLE)
                .unwrap_or(run.started_at);
        }
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
        assert_eq!(app.activity_tools.len(), 1);
        let run = &app.activity_tools[0];
        assert_eq!(run.name, "terminal.exec");
        assert_eq!(run.state, ToolRunState::Succeeded);
        assert_eq!(run.detail, "ok");
        assert_eq!(app.activity_phase, ToolActivityPhase::CurrentTurn);
    }

    #[test]
    fn ctrl_a_opens_activity_modal() {
        let mut app = app();

        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));

        assert_eq!(app.active_modal, Some(Modal::Activity));
        assert_eq!(app.status_line, "activity opened");
    }

    #[test]
    fn plan_command_opens_tabbed_plan_modal() {
        let mut app = app();

        assert!(app.run_local_tool_command("/plan"));
        assert_eq!(app.active_modal, Some(Modal::Plan));
        assert_eq!(app.plan_tab, PlanTab::Plan);

        app.handle_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE));
        assert_eq!(app.plan_tab, PlanTab::History);

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.active_modal, None);
    }

    #[test]
    fn plan_update_output_renders_inline_checklist() {
        let mut app = app();
        app.apply_plan_update_output(
            r#"{"summary":"Ship plan UI","items":[{"text":"Inspect current renderer","status":"done","evidence":["main.rs"]},{"text":"Render plan rows","status":"active"},{"text":"Run tests","status":"pending"}]}"#,
        )
        .unwrap();

        let Some(plan) = app.current_plan() else {
            panic!("expected current plan");
        };
        assert_eq!(plan.summary, "Ship plan UI");
        assert_eq!(plan.items.len(), 3);
        assert_eq!(plan.items[1].status, PlanItemStatus::Active);

        let lines = visible_transcript_lines(&app.transcript, None, None);
        let text = lines.iter().map(line_text).collect::<Vec<_>>();
        assert!(text.iter().any(|line| line.contains("plan")));
        assert!(text.iter().any(|line| line.contains("3 steps")));
        assert!(text.iter().any(|line| line.contains("Render plan rows")));
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

        let evidence_lines = plan_evidence_lines(Some(plan))
            .iter()
            .map(line_text)
            .collect::<Vec<_>>();
        assert!(
            evidence_lines
                .iter()
                .any(|line| line.contains("cargo check"))
        );
    }

    #[test]
    fn decision_request_output_renders_inline_and_in_planning_rail() {
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

        let rail = planning_rail_lines(&app, 42, 30)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>();
        assert!(rail.iter().any(|line| line.contains("Planning")));
        assert!(rail.iter().any(|line| line.contains("Decisions")));
        assert!(rail.iter().any(|line| line.contains("Choose storage")));
        assert!(rail.iter().any(|line| line.contains("transcript")));
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
    fn planning_rail_visibility_requires_width_and_state() {
        let mut app = app();
        assert!(!app.planning_rail_visible(Rect::new(0, 0, 120, 40)));

        app.apply_plan_update_output(
            r#"{"summary":"Plan","items":[{"text":"one","status":"active"}]}"#,
        )
        .unwrap();

        assert!(app.planning_rail_visible(Rect::new(0, 0, 120, 40)));
        assert!(!app.planning_rail_visible(Rect::new(0, 0, 80, 40)));
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
        })];

        let lines = visible_transcript_lines(&transcript, None, None);
        let text = lines.iter().map(line_text).collect::<Vec<_>>();

        assert_eq!(lines.len(), 3);
        assert!(text[0].contains("tools"));
        assert!(text[0].contains("1 call"));
        assert!(
            text[0].contains("⠁")
                || text[0].contains("⠃")
                || text[0].contains("⠇")
                || text[0].contains("⠧")
                || text[0].contains("⠷")
                || text[0].contains("⠿")
        );
        assert!(
            text[1].contains("edited 1 file")
                && (text[1].contains("⠁")
                    || text[1].contains("⠃")
                    || text[1].contains("⠇")
                    || text[1].contains("⠧")
                    || text[1].contains("⠷")
                    || text[1].contains("⠿"))
        );
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
        };

        let mut first = Vec::new();
        append_compact_tool_call_lines(&mut first, &run, RenderContext { animation_tick: 0 });
        let mut second = Vec::new();
        append_compact_tool_call_lines(&mut second, &run, RenderContext { animation_tick: 12 });

        let first_text = first.iter().map(line_text).collect::<Vec<_>>();
        let second_text = second.iter().map(line_text).collect::<Vec<_>>();

        assert_ne!(first_text[0], second_text[0]);
        assert_eq!(first_text[1], second_text[1]);
        assert!(!second_text[0].contains("⬤"));
        assert!(second_text[0].contains("⠧"));
        assert!(second_text[1].contains("running"));
    }

    #[test]
    fn tool_group_subtree_items_aggregate_related_calls() {
        let read_one = ToolRun {
            id: None,
            started_at: Instant::now(),
            pending_result: None,
            name: "terminal.exec".to_string(),
            summary: "$ sed -n '1,80p' README.md".to_string(),
            state: ToolRunState::Succeeded,
            detail: "done".to_string(),
            expanded: false,
        };
        let read_two = ToolRun {
            id: None,
            started_at: Instant::now(),
            pending_result: None,
            name: "file.read".to_string(),
            summary: "crates/medusa-tui/src/main.rs".to_string(),
            state: ToolRunState::Succeeded,
            detail: "read 20 lines".to_string(),
            expanded: false,
        };
        let edit = ToolRun {
            id: None,
            started_at: Instant::now(),
            pending_result: None,
            name: "file.patch".to_string(),
            summary: "apply patch".to_string(),
            state: ToolRunState::Failed,
            detail: "error: patch rejected".to_string(),
            expanded: false,
        };
        let runs = vec![&read_one, &read_two, &edit];

        let items = tool_group_subtree_items(&runs);

        assert_eq!(items.len(), 2);
        assert_eq!(items[0].summary(), "read 2 files");
        assert_eq!(items[1].summary(), "edited 1 file");
        assert_eq!(items[1].state, ToolRunState::Failed);
    }

    #[test]
    fn activity_detail_tabs_cycle_and_scroll() {
        let mut app = app();

        assert_eq!(app.activity_detail_tab, ToolDetailTab::Summary);

        app.next_activity_detail_tab();
        assert_eq!(app.activity_detail_tab, ToolDetailTab::Output);

        app.scroll_activity_detail_by(8);
        assert_eq!(app.activity_detail_scroll, 8);

        app.previous_activity_detail_tab();
        assert_eq!(app.activity_detail_tab, ToolDetailTab::Summary);
        assert_eq!(app.activity_detail_scroll, 0);
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
            }),
        ];

        let lines = visible_transcript_lines(&transcript, None, None);
        let text = lines.iter().map(line_text).collect::<Vec<_>>();

        assert_eq!(lines.len(), 5);
        assert!(text[0].contains("tools"));
        assert!(!text[0].contains("done"));
        assert!(text.iter().any(|line| line.contains("tested 1 command")));
        assert!(text.iter().any(|line| line.contains("edited 1 file")));
        assert!(text.iter().any(|line| line.contains("patch rejected")));
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
            }),
        ];

        let lines = visible_transcript_lines(&transcript, None, None);
        let text = lines.iter().map(line_text).collect::<Vec<_>>();

        assert_eq!(lines.len(), 4);
        assert!(text[0].contains("tools"));
        assert!(text[0].contains("2 calls"));
        assert!(text[0].contains("terminal x2"));
        assert!(text.iter().any(|line| line.contains("searched 1 query")));
        assert!(text.iter().any(|line| line.contains("read 1 file")));
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
        })];

        let lines = visible_transcript_lines(&transcript, None, Some(0));

        assert_eq!(lines.len(), 3);
        let text = lines.iter().map(line_text).collect::<Vec<_>>();
        assert!(!text[0].contains("done"));
        assert!(text[1].contains("ran 1 command"));
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
        })];

        let lines = visible_transcript_lines(&transcript, None, Some(0));

        assert_eq!(lines.len(), 6);
    }
}
