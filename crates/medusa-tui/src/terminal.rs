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
    cursor::Show,
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    style::force_color_output,
    terminal::{
        BeginSynchronizedUpdate, EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen,
        disable_raw_mode, enable_raw_mode, supports_keyboard_enhancement,
    },
};
use ratatui::{Terminal, backend::CrosstermBackend, symbols::border};

pub(crate) type Tui = Terminal<CrosstermBackend<io::Stdout>>;

static KEYBOARD_ENHANCED: AtomicBool = AtomicBool::new(false);

const ASCII_BORDER: border::Set<'static> = border::Set {
    top_left: "+",
    top_right: "+",
    bottom_left: "+",
    bottom_right: "+",
    vertical_left: "|",
    vertical_right: "|",
    horizontal_top: "-",
    horizontal_bottom: "-",
};

fn ascii_ui_enabled(term_program: Option<&str>, override_value: Option<&str>) -> bool {
    match override_value.map(|value| value.trim().to_ascii_lowercase()) {
        Some(value) if matches!(value.as_str(), "1" | "true" | "on" | "always") => true,
        Some(value) if matches!(value.as_str(), "0" | "false" | "off" | "never") => false,
        _ => term_program.is_some_and(|value| value.eq_ignore_ascii_case("Apple_Terminal")),
    }
}

pub(crate) fn ascii_ui() -> bool {
    ascii_ui_enabled(
        env::var("TERM_PROGRAM").ok().as_deref(),
        env::var("MEDUSA_ASCII").ok().as_deref(),
    )
}

pub(crate) fn ui_border_set() -> border::Set<'static> {
    if ascii_ui() {
        ASCII_BORDER
    } else {
        border::ROUNDED
    }
}

pub(crate) fn horizontal_rule(width: usize) -> String {
    ui_border_set().horizontal_top.repeat(width)
}

fn themed_color_output_enabled(override_value: Option<&str>) -> bool {
    !override_value.is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "never" | "off"
        )
    })
}

/// Restores the user's terminal if startup, rendering, or unwinding exits the
/// normal shutdown path after raw mode has been enabled.
pub(crate) struct TerminalRestoreGuard {
    armed: bool,
}

impl TerminalRestoreGuard {
    pub(crate) fn armed() -> Self {
        Self { armed: true }
    }

    pub(crate) fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TerminalRestoreGuard {
    fn drop(&mut self) {
        if self.armed {
            restore_terminal_best_effort();
        }
    }
}

pub(crate) fn init_terminal() -> Result<Tui> {
    // Medusa is a full-screen themed application, so inherited NO_COLOR values
    // from a parent process must not silently collapse every theme to the same
    // monochrome UI. Users who intentionally need monochrome output can opt out
    // with MEDUSA_COLOR=never.
    force_color_output(themed_color_output_enabled(
        env::var("MEDUSA_COLOR").ok().as_deref(),
    ));
    enable_raw_mode()?;
    let result = (|| -> Result<Tui> {
        let mut stdout = io::stdout();
        execute!(
            stdout,
            EnterAlternateScreen,
            EnableBracketedPaste,
            EnableMouseCapture
        )?;

        // Kitty keyboard protocol (Ghostty, Kitty, WezTerm, foot):
        // disambiguates modified keys so Shift+Enter / Alt+Enter reach the
        // composer instead of collapsing to plain Enter.
        if matches!(supports_keyboard_enhancement(), Ok(true)) {
            execute!(
                stdout,
                PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
            )?;
            KEYBOARD_ENHANCED.store(true, Ordering::Relaxed);
        }

        let backend = CrosstermBackend::new(stdout);
        Ok(Terminal::new(backend)?)
    })();
    if result.is_err() {
        restore_terminal_best_effort();
    }
    result
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

fn restore_terminal_best_effort() {
    let _ = disable_raw_mode();
    let mut stdout = io::stdout();
    if KEYBOARD_ENHANCED.swap(false, Ordering::Relaxed) {
        let _ = execute!(stdout, PopKeyboardEnhancementFlags);
    }
    let _ = execute!(
        stdout,
        DisableMouseCapture,
        DisableBracketedPaste,
        LeaveAlternateScreen,
        Show
    );
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

#[cfg(test)]
mod tests {
    use super::{ascii_ui_enabled, themed_color_output_enabled};

    #[test]
    fn themed_tui_enables_color_by_default() {
        assert!(themed_color_output_enabled(None));
        assert!(themed_color_output_enabled(Some("always")));
        assert!(themed_color_output_enabled(Some("auto")));
    }

    #[test]
    fn explicit_monochrome_override_disables_color() {
        for value in ["never", "off", "false", "0", " NEVER "] {
            assert!(!themed_color_output_enabled(Some(value)));
        }
    }

    #[test]
    fn apple_terminal_uses_ascii_unless_overridden() {
        assert!(ascii_ui_enabled(Some("Apple_Terminal"), None));
        assert!(!ascii_ui_enabled(Some("Apple_Terminal"), Some("off")));
        assert!(ascii_ui_enabled(Some("ghostty"), Some("on")));
        assert!(!ascii_ui_enabled(Some("ghostty"), None));
    }
}
