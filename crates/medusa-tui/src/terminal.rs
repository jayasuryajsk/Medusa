use std::{
    env, io,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use color_eyre::eyre::{Result, WrapErr, bail};
use crossterm::{
    event::{DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};

pub(crate) type Tui = Terminal<CrosstermBackend<io::Stdout>>;

pub(crate) fn init_terminal() -> Result<Tui> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture
    )?;
    let backend = CrosstermBackend::new(stdout);
    Ok(Terminal::new(backend)?)
}

pub(crate) fn restore_terminal(terminal: &mut Tui) -> Result<()> {
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        DisableBracketedPaste,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;
    Ok(())
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
