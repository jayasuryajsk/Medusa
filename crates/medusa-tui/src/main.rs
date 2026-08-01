use color_eyre::eyre::Result;

mod animation;
mod app;
mod cli;
mod config;
mod constants;
mod markdown;
mod render;
mod session_memory;
mod slash;
mod styles;
mod terminal;
mod types;
mod util;

use app::App;
use cli::{StartupCommand, parse_args, run_auth, run_headless};
use terminal::{
    TerminalRestoreGuard, init_terminal, relaunch_current_executable, restore_terminal,
};

fn main() -> Result<()> {
    color_eyre::install()?;

    let startup_session = match parse_args()? {
        StartupCommand::Tui(startup_session) => startup_session,
        StartupCommand::Headless(options) => return run_headless(options),
        StartupCommand::Auth(command) => return run_auth(command),
        StartupCommand::Print(text) => {
            println!("{text}");
            return Ok(());
        }
    };

    let mut terminal = init_terminal()?;
    let mut restore_guard = TerminalRestoreGuard::armed();
    let mut app = App::new(startup_session)?;
    let app_result = app.run(&mut terminal);
    let restart_requested = app.restart_requested;
    // Reap MCP server children deterministically (stdin EOF, then a bounded
    // kill) instead of leaning on process exit.
    app.mcp.shutdown();
    restore_terminal(&mut terminal)?;
    restore_guard.disarm();

    app_result?;

    if restart_requested {
        relaunch_current_executable()?;
    }

    Ok(())
}
