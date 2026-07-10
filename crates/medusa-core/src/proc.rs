//! Cancellable child-process runner shared by post-edit verification and
//! foreground `terminal_exec`. Generalizes the old `verify.rs`
//! `run_with_timeout`: the child runs in its own process group, output is
//! drained on reader threads, and a 40ms `try_wait` poll checks both the
//! deadline and the turn's [`CancelToken`].

use std::{
    io::Read,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use crate::cancel::CancelToken;

const POLL_INTERVAL: Duration = Duration::from_millis(40);
/// SIGTERM → grace → SIGKILL: long enough for shells to reap their children,
/// short enough that cancellation still feels instant.
const KILL_GRACE: Duration = Duration::from_millis(500);

pub(crate) struct CommandOutcome {
    pub(crate) success: bool,
    /// Exit code when the child exited normally (None on signal death).
    pub(crate) code: Option<i32>,
    pub(crate) timed_out: bool,
    pub(crate) cancelled: bool,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
}

/// Run to completion, the deadline, or cancellation — whichever comes first.
/// Output is drained on reader threads so a chatty child can never deadlock
/// against a full pipe. `timeout: None` means no deadline.
pub(crate) fn run_command(
    mut command: Command,
    timeout: Option<Duration>,
    cancel: &CancelToken,
) -> std::io::Result<CommandOutcome> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Own process group, so killing on cancel/timeout reaches the whole
        // tree (`$SHELL -lc` grandchildren included), not just the shell.
        command.process_group(0);
    }
    let mut child = command.spawn()?;

    let stdout_reader = drain_pipe(child.stdout.take());
    let stderr_reader = drain_pipe(child.stderr.take());

    let deadline = timeout.map(|timeout| Instant::now() + timeout);
    let (success, code, timed_out, cancelled) = loop {
        match child.try_wait()? {
            Some(status) => break (status.success(), status.code(), false, false),
            None if cancel.is_cancelled() => {
                kill_child_tree(&mut child);
                break (false, None, false, true);
            }
            None if deadline.is_some_and(|deadline| Instant::now() >= deadline) => {
                kill_child_tree(&mut child);
                break (false, None, true, false);
            }
            None => thread::sleep(POLL_INTERVAL),
        }
    };

    Ok(CommandOutcome {
        success,
        code,
        timed_out,
        cancelled,
        stdout: stdout_reader.join().unwrap_or_default(),
        stderr: stderr_reader.join().unwrap_or_default(),
    })
}

fn drain_pipe<R: Read + Send + 'static>(pipe: Option<R>) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let mut buffer = String::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_string(&mut buffer);
        }
        buffer
    })
}

/// Terminate the child's whole process group: SIGTERM first so shells can
/// clean up, a short grace, then SIGKILL for anything that ignored it.
///
/// This needs `libc`: std's `Child::kill` sends SIGKILL to the direct child
/// only and cannot signal a process group at all, so `$SHELL -lc`
/// grandchildren would survive it.
#[cfg(unix)]
fn kill_child_tree(child: &mut Child) {
    let pgid = child.id() as libc::pid_t;
    unsafe { libc::killpg(pgid, libc::SIGTERM) };

    let grace_deadline = Instant::now() + KILL_GRACE;
    while Instant::now() < grace_deadline {
        match child.try_wait() {
            Ok(None) => thread::sleep(POLL_INTERVAL),
            // Exited (or unwaitable) — stop gracing, sweep with SIGKILL below.
            _ => break,
        }
    }

    unsafe { libc::killpg(pgid, libc::SIGKILL) };
    let _ = child.wait();
}

#[cfg(not(unix))]
fn kill_child_tree(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shell_command(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.arg("-c").arg(script);
        command
    }

    #[test]
    fn runs_to_completion_and_captures_output() {
        let outcome = run_command(
            shell_command("printf out; printf err >&2"),
            None,
            &CancelToken::default(),
        )
        .unwrap();

        assert!(outcome.success);
        assert_eq!(outcome.code, Some(0));
        assert!(!outcome.timed_out);
        assert!(!outcome.cancelled);
        assert_eq!(outcome.stdout, "out");
        assert_eq!(outcome.stderr, "err");
    }

    #[test]
    fn timeout_kills_hung_commands() {
        let started = Instant::now();
        let outcome = run_command(
            shell_command("sleep 30"),
            Some(Duration::from_millis(300)),
            &CancelToken::default(),
        )
        .unwrap();

        assert!(outcome.timed_out);
        assert!(!outcome.cancelled);
        assert!(!outcome.success);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn cancellation_kills_hung_commands_promptly() {
        let cancel = CancelToken::new();
        let canceller = cancel.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            canceller.cancel();
        });

        let started = Instant::now();
        let outcome = run_command(shell_command("sleep 30"), None, &cancel).unwrap();

        assert!(outcome.cancelled);
        assert!(!outcome.timed_out);
        assert!(!outcome.success);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_kills_the_whole_process_group() {
        let cancel = CancelToken::new();
        let canceller = cancel.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(150));
            canceller.cancel();
        });

        // The shell prints its background child's pid, then blocks on it —
        // exactly the shape `Child::kill` alone would leak.
        let outcome =
            run_command(shell_command("sleep 30 & echo $!; wait"), None, &cancel).unwrap();
        assert!(outcome.cancelled);

        let grandchild: i32 = outcome
            .stdout
            .trim()
            .parse()
            .expect("shell should print the grandchild pid");
        // The grandchild must die with the group; allow the OS a moment to
        // deliver the signal and reap.
        let deadline = Instant::now() + Duration::from_secs(2);
        while unsafe { libc::kill(grandchild, 0) } == 0 {
            assert!(
                Instant::now() < deadline,
                "grandchild sleep survived the process-group kill"
            );
            thread::sleep(Duration::from_millis(25));
        }
    }
}
