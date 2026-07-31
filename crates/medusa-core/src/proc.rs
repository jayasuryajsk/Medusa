//! Cancellable child-process runner shared by post-edit verification and
//! foreground `terminal_exec`. Generalizes the old `verify.rs`
//! `run_with_timeout`: the child runs in its own process group, output is
//! drained on reader threads, and a 40ms `try_wait` poll checks both the
//! deadline and the turn's [`CancelToken`].

use std::{
    collections::VecDeque,
    io::Read,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use crate::cancel::CancelToken;

const POLL_INTERVAL: Duration = Duration::from_millis(40);
const MAX_CAPTURED_OUTPUT_BYTES: usize = 4 * 1024 * 1024;
const OUTPUT_TAIL_BYTES: usize = 1024 * 1024;
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
    command: Command,
    timeout: Option<Duration>,
    cancel: &CancelToken,
) -> std::io::Result<CommandOutcome> {
    let mut child = spawn_command(command)?;
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

/// Spawn a command with the same pipe and process-group discipline used by
/// foreground execution. Background jobs use this so their output is bounded
/// and a later explicit kill can reach the whole shell process tree.
pub(crate) fn spawn_command(mut command: Command) -> std::io::Result<Child> {
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
    command.spawn()
}

/// Wait for an already managed child without a deadline. Pipes are drained
/// through the bounded capture path, so a long-lived background task cannot
/// grow Medusa's memory without limit.
pub(crate) fn wait_for_child(mut child: Child) -> std::io::Result<CommandOutcome> {
    let stdout_reader = drain_pipe(child.stdout.take());
    let stderr_reader = drain_pipe(child.stderr.take());
    let status = child.wait()?;

    Ok(CommandOutcome {
        success: status.success(),
        code: status.code(),
        timed_out: false,
        cancelled: false,
        stdout: stdout_reader.join().unwrap_or_default(),
        stderr: stderr_reader.join().unwrap_or_default(),
    })
}

fn drain_pipe<R: Read + Send + 'static>(pipe: Option<R>) -> thread::JoinHandle<String> {
    thread::spawn(move || pipe.map(capture_reader).unwrap_or_default())
}

fn capture_reader(mut reader: impl Read) -> String {
    let head_limit = MAX_CAPTURED_OUTPUT_BYTES - OUTPUT_TAIL_BYTES;
    let mut head = Vec::with_capacity(head_limit.min(64 * 1024));
    let mut tail = VecDeque::with_capacity(OUTPUT_TAIL_BYTES);
    let mut chunk = [0u8; 16 * 1024];
    let mut total = 0usize;

    loop {
        let Ok(read) = reader.read(&mut chunk) else {
            break;
        };
        if read == 0 {
            break;
        }
        total = total.saturating_add(read);

        let mut offset = 0;
        if head.len() < head_limit {
            let keep = (head_limit - head.len()).min(read);
            head.extend_from_slice(&chunk[..keep]);
            offset = keep;
        }
        for byte in &chunk[offset..read] {
            if tail.len() == OUTPUT_TAIL_BYTES {
                tail.pop_front();
            }
            tail.push_back(*byte);
        }
    }

    let mut output = String::from_utf8_lossy(&head).into_owned();
    if total > MAX_CAPTURED_OUTPUT_BYTES {
        let omitted = total - MAX_CAPTURED_OUTPUT_BYTES;
        output.push_str(&format!(
            "\n[medusa truncated {omitted} bytes of process output]\n"
        ));
    }
    let tail = tail.into_iter().collect::<Vec<_>>();
    output.push_str(&String::from_utf8_lossy(&tail));
    output
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

    #[test]
    fn process_output_is_bounded_but_keeps_the_tail() {
        let mut input = vec![b'a'; MAX_CAPTURED_OUTPUT_BYTES + 4096];
        input.extend_from_slice(b"important tail");

        let output = capture_reader(std::io::Cursor::new(input));

        assert!(output.contains("[medusa truncated"));
        assert!(output.ends_with("important tail"));
        assert!(output.len() < MAX_CAPTURED_OUTPUT_BYTES + 256);
    }

    #[test]
    fn managed_background_child_drains_and_bounds_output() {
        let child = spawn_command(shell_command(
            "yes x | head -c 5000000; printf 'important tail'",
        ))
        .unwrap();

        let outcome = wait_for_child(child).unwrap();

        assert!(outcome.success);
        assert!(outcome.stdout.contains("[medusa truncated"));
        assert!(outcome.stdout.ends_with("important tail"));
        assert!(outcome.stdout.len() < MAX_CAPTURED_OUTPUT_BYTES + 256);
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
