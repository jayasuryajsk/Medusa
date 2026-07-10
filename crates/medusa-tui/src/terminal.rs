use std::{
    env, io,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use std::sync::atomic::{AtomicBool, Ordering};

use color_eyre::eyre::{Result, WrapErr, bail};
use crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{
        BeginSynchronizedUpdate, EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen,
        disable_raw_mode, enable_raw_mode, supports_keyboard_enhancement,
    },
};
use ratatui::{Terminal, backend::CrosstermBackend};

pub(crate) type Tui = Terminal<CrosstermBackend<io::Stdout>>;

static KEYBOARD_ENHANCED: AtomicBool = AtomicBool::new(false);

pub(crate) fn init_terminal() -> Result<Tui> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture
    )?;

    // Kitty keyboard protocol (Ghostty, Kitty, WezTerm, foot): disambiguates
    // modified keys so Shift+Enter / Alt+Enter reach the composer instead of
    // collapsing to plain Enter.
    if matches!(supports_keyboard_enhancement(), Ok(true)) {
        execute!(
            stdout,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )?;
        KEYBOARD_ENHANCED.store(true, Ordering::Relaxed);
    }

    let backend = CrosstermBackend::new(stdout);
    Ok(Terminal::new(backend)?)
}

pub(crate) fn restore_terminal(terminal: &mut Tui) -> Result<()> {
    disable_raw_mode()?;
    if KEYBOARD_ENHANCED.swap(false, Ordering::Relaxed) {
        execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags)?;
    }
    execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        DisableBracketedPaste,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;
    Ok(())
}

/// Run `draw` inside a synchronized-output block (DEC mode 2026). Terminals
/// that support it (Ghostty, Kitty, WezTerm, iTerm2) commit the frame
/// atomically — no mid-frame tearing; others ignore the markers.
pub(crate) fn draw_synchronized(
    terminal: &mut Tui,
    draw: impl FnOnce(&mut Tui) -> io::Result<()>,
) -> io::Result<()> {
    execute!(terminal.backend_mut(), BeginSynchronizedUpdate)?;
    let result = draw(terminal);
    // Always release the sync guard, even if the draw failed — a stuck
    // BeginSynchronizedUpdate freezes the terminal's screen updates.
    let end = execute!(terminal.backend_mut(), EndSynchronizedUpdate);
    result.and(end)
}

pub(crate) fn maybe_rebuild_before_reload(executable: &Path) -> Result<()> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let Some(workspace_root) = manifest_dir.parent().and_then(Path::parent) else {
        return Ok(());
    };

    if !workspace_root.join("Cargo.toml").is_file() {
        return Ok(());
    }

    let workspace_target = workspace_root.join("target");
    if !executable.starts_with(&workspace_target) {
        return Ok(());
    }

    let profile = executable
        .strip_prefix(&workspace_target)
        .ok()
        .and_then(|relative| relative.components().next())
        .and_then(|component| component.as_os_str().to_str())
        .unwrap_or("debug");

    let mut command = Command::new("cargo");
    command
        .arg("build")
        .arg("-p")
        .arg("medusa-tui")
        .current_dir(workspace_root)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    if profile == "release" {
        command.arg("--release");
    }

    let status = command.status().wrap_err("failed to rebuild Medusa")?;
    if !status.success() {
        bail!("Medusa rebuild failed; reload aborted");
    }

    Ok(())
}

#[cfg(unix)]
pub(crate) fn relaunch_current_executable() -> Result<()> {
    let executable = env::current_exe().wrap_err("failed to locate current executable")?;
    let selected_theme = env::var_os("MEDUSA_RELOAD_THEME");
    maybe_rebuild_before_reload(&executable)?;
    let mut command = Command::new(executable);
    command.arg("continue");
    if let Some(theme) = selected_theme {
        command.env("MEDUSA_THEME", theme);
    }
    let error = command.exec();
    Err(error).wrap_err("failed to reload Medusa")
}

#[cfg(not(unix))]
pub(crate) fn relaunch_current_executable() -> Result<()> {
    let executable = env::current_exe().wrap_err("failed to locate current executable")?;
    let selected_theme = env::var_os("MEDUSA_RELOAD_THEME");
    maybe_rebuild_before_reload(&executable)?;
    let mut command = Command::new(executable);
    command.arg("continue");
    if let Some(theme) = selected_theme {
        command.env("MEDUSA_THEME", theme);
    }
    command.spawn().wrap_err("failed to reload Medusa")?;
    Ok(())
}
