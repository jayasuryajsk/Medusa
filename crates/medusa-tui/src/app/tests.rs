use super::*;

use std::sync::atomic::Ordering;

use ratatui::style::Color;

use crate::cli::{
    HELP_TEXT, HeadlessOptions, RUN_HELP_TEXT, StartupCommand, VERSION_TEXT, parse_startup_command,
};
use crate::markdown::markdown_content_lines;

use medusa_core::session::{compact_session_id, normalize_session_name, read_session_file};
use medusa_core::workflow::SubagentToolPolicy;

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

fn write_saved_workflow(app: &App, name: &str, source: &str) {
    let directory = app.tools.workspace().join(".medusa/workflows");
    fs::create_dir_all(&directory).unwrap();
    fs::write(directory.join(format!("{name}.js")), source).unwrap();
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

mod composer;
mod history;
mod plan_tools;
mod preferences;
mod runtime;
mod transcript;
mod workflows;
