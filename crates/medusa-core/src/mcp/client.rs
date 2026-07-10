//! One stdio MCP server connection: newline-delimited JSON-RPC 2.0 over the
//! child's stdin/stdout (no Content-Length framing — that is LSP, not MCP).
//! A reader thread correlates responses to blocked callers through an
//! id-keyed pending map; a dedicated writer thread owns the child's stdin so
//! a server that stops reading it can never wedge a turn thread on a blocking
//! write. stderr is always drained into a bounded ring buffer so a chatty
//! server can never deadlock against a full pipe.

use std::{
    collections::{HashMap, VecDeque},
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicI64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use color_eyre::eyre::{Result, WrapErr, bail, eyre};
use serde_json::{Value, json};

use super::McpServerConfig;
use crate::cancel::CancelToken;

/// Protocol revisions this client accepts back from `initialize`.
const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];
/// The revision we request.
const REQUESTED_PROTOCOL_VERSION: &str = "2025-06-18";
/// Hard cap on `tools/list` pagination so a misbehaving server that keeps
/// returning `nextCursor` cannot spin forever.
const MAX_TOOL_LIST_PAGES: usize = 64;
/// stderr ring buffer byte budget.
const STDERR_RING_BYTES: usize = 8 * 1024;
/// Grace given to the child between stdin close and kill on Drop.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
const SHUTDOWN_POLL: Duration = Duration::from_millis(40);
/// Slice length for the response wait: the caller re-checks the cancel token
/// (and the overall deadline) at least this often, so Esc interrupts an
/// in-flight MCP call within ~100ms instead of after the whole tool timeout.
const REQUEST_POLL: Duration = Duration::from_millis(100);

/// Result of one `tools/call`: joined text content plus the server's
/// `isError` flag. Non-text content items become placeholders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpToolOutcome {
    pub text: String,
    pub is_error: bool,
}

/// Message to the per-connection writer thread. `Close` drops the child's
/// stdin (EOF) even while other writer-channel senders (e.g. the reader
/// thread's ping-reply lane) are still alive.
enum WriterMsg {
    Line(Vec<u8>),
    Close,
}

/// Bounded line ring: keeps the newest lines within a byte budget.
struct RingBuffer {
    lines: VecDeque<String>,
    bytes: usize,
}

impl RingBuffer {
    fn new() -> Self {
        Self {
            lines: VecDeque::new(),
            bytes: 0,
        }
    }

    fn push_line(&mut self, line: &str) {
        let line: String = line.chars().take(512).collect();
        self.bytes += line.len();
        self.lines.push_back(line);
        while self.bytes > STDERR_RING_BYTES {
            let Some(dropped) = self.lines.pop_front() else {
                break;
            };
            self.bytes -= dropped.len();
        }
    }

    fn tail(&self, max_lines: usize) -> String {
        let skip = self.lines.len().saturating_sub(max_lines);
        self.lines
            .iter()
            .skip(skip)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    }
}

pub(crate) struct McpConnection {
    server_name: String,
    child: Mutex<Child>,
    /// Writer lane: every outbound line is handed to the writer thread so no
    /// turn thread ever blocks on a `write_all` to a wedged pipe.
    writer: mpsc::Sender<WriterMsg>,
    pending: Arc<Mutex<HashMap<i64, mpsc::Sender<Value>>>>,
    next_id: AtomicI64,
    alive: Arc<AtomicBool>,
    /// Set by `notifications/tools/list_changed`; the registry refreshes the
    /// tool cache on the next schema build.
    tools_stale: Arc<AtomicBool>,
    stderr_tail: Arc<Mutex<RingBuffer>>,
}

impl McpConnection {
    /// Spawn the server, run the `initialize` handshake, and send
    /// `notifications/initialized`. The whole handshake shares one timeout and
    /// polls `cancel` so Esc during a slow connect aborts promptly.
    pub(crate) fn connect(
        name: &str,
        config: &McpServerConfig,
        workspace: &Path,
        timeout: Duration,
        cancel: &CancelToken,
    ) -> Result<Self> {
        let mut command = Command::new(&config.command);
        command
            .args(&config.args)
            .envs(&config.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if workspace.is_dir() {
            command.current_dir(workspace);
        }

        let mut child = command.spawn().wrap_err_with(|| {
            format!(
                "failed to start MCP server `{name}` (command: {})",
                config.command
            )
        })?;

        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let alive = Arc::new(AtomicBool::new(true));
        let tools_stale = Arc::new(AtomicBool::new(false));
        let stderr_tail = Arc::new(Mutex::new(RingBuffer::new()));

        // The writer thread owns stdin; callers reach it only through the
        // channel, so a blocking write can never wedge a turn thread.
        let (writer_tx, writer_rx) = mpsc::channel::<WriterMsg>();
        if let Some(stdin) = stdin {
            let alive = alive.clone();
            thread::spawn(move || writer_loop(stdin, &writer_rx, &alive));
        }

        if let Some(stdout) = stdout {
            let writer = writer_tx.clone();
            let pending = pending.clone();
            let alive = alive.clone();
            let tools_stale = tools_stale.clone();
            let ring = stderr_tail.clone();
            thread::spawn(move || {
                reader_loop(stdout, &writer, &pending, &alive, &tools_stale, &ring)
            });
        }
        if let Some(stderr) = stderr {
            let ring = stderr_tail.clone();
            let debug_log = debug_log_path(workspace, name);
            thread::spawn(move || stderr_loop(stderr, &ring, debug_log.as_deref()));
        }

        let connection = Self {
            server_name: name.to_string(),
            child: Mutex::new(child),
            writer: writer_tx,
            pending,
            next_id: AtomicI64::new(1),
            alive,
            tools_stale,
            stderr_tail,
        };

        connection
            .handshake(name, timeout, cancel)
            .map_err(|error| {
                let tail = connection.stderr_tail();
                match tail.is_empty() {
                    true => error,
                    false => error.wrap_err(format!("server stderr tail:\n{tail}")),
                }
            })?;
        Ok(connection)
    }

    fn handshake(&self, name: &str, timeout: Duration, cancel: &CancelToken) -> Result<()> {
        let result = self.request(
            "initialize",
            json!({
                "protocolVersion": REQUESTED_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {
                    "name": "medusa",
                    "version": env!("CARGO_PKG_VERSION"),
                },
            }),
            timeout,
            cancel,
        )?;

        let version = result
            .get("protocolVersion")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !SUPPORTED_PROTOCOL_VERSIONS.contains(&version) {
            bail!(
                "MCP server `{name}` negotiated unsupported protocol version {version:?} (supported: {})",
                SUPPORTED_PROTOCOL_VERSIONS.join(", ")
            );
        }

        self.notify("notifications/initialized", json!({}))
    }

    /// `tools/list`, following `nextCursor` pagination to completion. Returns
    /// the raw tool objects; the registry namespaces them.
    pub(crate) fn list_tools(&self, timeout: Duration, cancel: &CancelToken) -> Result<Vec<Value>> {
        let deadline = Instant::now() + timeout;
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;

        for _ in 0..MAX_TOOL_LIST_PAGES {
            let params = match &cursor {
                Some(cursor) => json!({ "cursor": cursor }),
                None => json!({}),
            };
            let remaining = deadline
                .saturating_duration_since(Instant::now())
                .max(Duration::from_millis(1));
            let result = self.request("tools/list", params, remaining, cancel)?;
            if let Some(page) = result.get("tools").and_then(Value::as_array) {
                tools.extend(page.iter().cloned());
            }
            match result.get("nextCursor").and_then(Value::as_str) {
                Some(next) if !next.is_empty() => cursor = Some(next.to_string()),
                _ => return Ok(tools),
            }
        }

        bail!(
            "MCP server `{}` returned more than {MAX_TOOL_LIST_PAGES} tools/list pages; giving up",
            self.server_name
        );
    }

    /// `tools/call` with the given per-call timeout, polling `cancel`.
    pub(crate) fn call_tool(
        &self,
        tool: &str,
        arguments: &Value,
        timeout: Duration,
        cancel: &CancelToken,
    ) -> Result<McpToolOutcome> {
        let result = self.request(
            "tools/call",
            json!({ "name": tool, "arguments": arguments }),
            timeout,
            cancel,
        )?;
        Ok(parse_tool_result(&result))
    }

    /// Send one request and block on the correlated response. The wait is
    /// sliced into [`REQUEST_POLL`] steps so the caller's cancel token and the
    /// overall timeout are both honoured. A timeout removes the pending entry
    /// so the eventual late reply is discarded harmlessly by the reader thread.
    fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
        cancel: &CancelToken,
    ) -> Result<Value> {
        if !self.is_alive() {
            bail!(
                "MCP server `{}` connection is closed ({method} not sent)",
                self.server_name
            );
        }

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = mpsc::channel();
        lock_unpoisoned(&self.pending).insert(id, sender);

        let message = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        // Hand the line to the writer thread. `send` never blocks on the child;
        // a wedged pipe simply means no reply arrives and the wait below times
        // out (or cancels) instead of the turn thread parking on `write_all`.
        if self
            .writer
            .send(WriterMsg::Line(encode_line(&message)))
            .is_err()
        {
            lock_unpoisoned(&self.pending).remove(&id);
            self.alive.store(false, Ordering::SeqCst);
            bail!(
                "failed to write to MCP server `{}`: writer lane closed ({method})",
                self.server_name
            );
        }

        let deadline = Instant::now() + timeout;
        loop {
            match receiver.recv_timeout(REQUEST_POLL) {
                Ok(reply) => {
                    if let Some(error) = reply.get("error") {
                        let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
                        let message = error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown error");
                        bail!(
                            "MCP server `{}` returned error {code} for {method}: {message}",
                            self.server_name
                        );
                    }
                    return Ok(reply.get("result").cloned().unwrap_or(Value::Null));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    // Poll the cancel token first so Esc unwinds the turn
                    // promptly instead of waiting out the full tool timeout.
                    if cancel.is_cancelled() {
                        lock_unpoisoned(&self.pending).remove(&id);
                        cancel.bail_if_cancelled()?;
                    }
                    if Instant::now() >= deadline {
                        // Discard the pending entry: if the reply ever arrives
                        // the reader finds no waiter for the id and drops it.
                        lock_unpoisoned(&self.pending).remove(&id);
                        bail!(
                            "MCP {method} on server `{}` timed out after {}s",
                            self.server_name,
                            timeout.as_secs().max(1)
                        );
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    lock_unpoisoned(&self.pending).remove(&id);
                    bail!(
                        "MCP server `{}` closed the connection during {method}",
                        self.server_name
                    );
                }
            }
        }
    }

    /// Fire-and-forget notification (no id, no response expected).
    fn notify(&self, method: &str, params: Value) -> Result<()> {
        let message = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        self.writer
            .send(WriterMsg::Line(encode_line(&message)))
            .map_err(|_| {
                eyre!(
                    "failed to notify MCP server `{}` ({method}): writer lane closed",
                    self.server_name
                )
            })
    }

    pub(crate) fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    /// OS pid of the server process (tests assert the child is reaped).
    #[cfg(test)]
    pub(crate) fn pid(&self) -> u32 {
        lock_unpoisoned(&self.child).id()
    }

    /// Whether the server announced a tool-list change since the last cache
    /// refresh; reading clears the flag.
    pub(crate) fn take_tools_stale(&self) -> bool {
        self.tools_stale.swap(false, Ordering::SeqCst)
    }

    pub(crate) fn stderr_tail(&self) -> String {
        lock_unpoisoned(&self.stderr_tail).tail(8)
    }
}

impl Drop for McpConnection {
    /// Polite shutdown: tell the writer thread to close stdin (spec-compliant
    /// servers exit on EOF), poll `try_wait` briefly, then kill. `kill()` only
    /// reaches the direct child — an npx-style grandchild can outlive us
    /// (documented v1 limit).
    fn drop(&mut self) {
        // Force EOF even though the reader thread still holds a writer sender:
        // `Close` makes the writer thread drop the child's stdin regardless.
        let _ = self.writer.send(WriterMsg::Close);
        let mut child = lock_unpoisoned(&self.child);
        let deadline = Instant::now() + SHUTDOWN_GRACE;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() < deadline => thread::sleep(SHUTDOWN_POLL),
                _ => break,
            }
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// Mutex lock that shrugs off poisoning: MCP state stays usable even if a
/// holder panicked (the data is plain maps/buffers, always consistent).
fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Compact one JSON message into a single newline-terminated line. serde_json
/// compact encoding never emits raw newlines, so one message is one line.
fn encode_line(message: &Value) -> Vec<u8> {
    let mut line = serde_json::to_vec(message).unwrap_or_default();
    line.push(b'\n');
    line
}

/// Owns the child's stdin and drains the writer channel. A blocking
/// `write_all` here can wedge only this background thread, never a turn
/// thread; `Close`/write-error both drop stdin, sending the child EOF.
fn writer_loop(mut stdin: ChildStdin, rx: &mpsc::Receiver<WriterMsg>, alive: &AtomicBool) {
    while let Ok(msg) = rx.recv() {
        match msg {
            WriterMsg::Line(bytes) => {
                if stdin
                    .write_all(&bytes)
                    .and_then(|()| stdin.flush())
                    .is_err()
                {
                    alive.store(false, Ordering::SeqCst);
                    break;
                }
            }
            WriterMsg::Close => break,
        }
    }
    // Dropping `stdin` on the way out closes the child's input (EOF).
}

fn reader_loop(
    stdout: std::process::ChildStdout,
    writer: &mpsc::Sender<WriterMsg>,
    pending: &Mutex<HashMap<i64, mpsc::Sender<Value>>>,
    alive: &AtomicBool,
    tools_stale: &AtomicBool,
    ring: &Mutex<RingBuffer>,
) {
    let reader = BufReader::new(stdout);
    for line in reader.lines() {
        let Ok(line) = line else { break };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Spec violation but common (npx wrappers print banners): skip
        // unparseable stdout lines instead of killing the connection.
        let Ok(message) = serde_json::from_str::<Value>(trimmed) else {
            lock_unpoisoned(ring).push_line(&format!("[stdout] skipped non-JSON line: {trimmed}"));
            continue;
        };

        let method = message.get("method").and_then(Value::as_str);
        let id = message.get("id").filter(|id| !id.is_null());
        match (method, id) {
            // Server-initiated `ping`: the MCP spec requires the receiver to
            // respond promptly with an empty result. Refusing it would make
            // keepalive servers treat us as dead and drop the connection.
            (Some("ping"), Some(id)) => {
                let reply = json!({ "jsonrpc": "2.0", "id": id, "result": {} });
                let _ = writer.send(WriterMsg::Line(encode_line(&reply)));
            }
            // Other server-initiated requests (sampling/roots/elicitation):
            // refuse with -32601 so the server never hangs waiting on us.
            (Some(method), Some(id)) => {
                let reply = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": -32601,
                        "message": format!("medusa does not support server-initiated {method}"),
                    },
                });
                let _ = writer.send(WriterMsg::Line(encode_line(&reply)));
            }
            (Some("notifications/tools/list_changed"), None) => {
                tools_stale.store(true, Ordering::SeqCst);
            }
            // Other notifications (message/progress/cancelled) are discarded.
            (Some(_), None) => {}
            // Response: route to the waiting caller; unknown ids are late
            // replies whose waiter already timed out — drop them.
            (None, Some(id)) => {
                if let Some(id) = id.as_i64()
                    && let Some(sender) = lock_unpoisoned(pending).remove(&id)
                {
                    let _ = sender.send(message);
                }
            }
            (None, None) => {}
        }
    }

    // EOF: the server is gone. Fail every in-flight caller by dropping their
    // senders (recv sees Disconnected → "closed the connection").
    alive.store(false, Ordering::SeqCst);
    lock_unpoisoned(pending).clear();
}

fn stderr_loop(
    stderr: std::process::ChildStderr,
    ring: &Mutex<RingBuffer>,
    debug_log: Option<&Path>,
) {
    let mut log = debug_log.and_then(|path| {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok()?;
        }
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok()
    });

    let reader = BufReader::new(stderr);
    for line in reader.lines() {
        let Ok(line) = line else { break };
        if let Some(log) = log.as_mut() {
            let _ = writeln!(log, "{line}");
        }
        lock_unpoisoned(ring).push_line(&line);
    }
}

/// `.medusa/logs/mcp-<server>.log` when `MEDUSA_MCP_DEBUG=1`, else None.
fn debug_log_path(workspace: &Path, server: &str) -> Option<std::path::PathBuf> {
    let enabled = std::env::var("MEDUSA_MCP_DEBUG")
        .map(|value| matches!(value.trim(), "1" | "true" | "on"))
        .unwrap_or(false);
    enabled.then(|| {
        workspace.join(".medusa").join("logs").join(format!(
            "mcp-{}.log",
            super::sanitize_name_component(server)
        ))
    })
}

fn parse_tool_result(result: &Value) -> McpToolOutcome {
    let is_error = result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let mut parts = Vec::new();
    if let Some(content) = result.get("content").and_then(Value::as_array) {
        for item in content {
            match item.get("type").and_then(Value::as_str) {
                Some("text") => parts.push(
                    item.get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                ),
                Some(other) => parts.push(format!("[non-text content: {other} omitted]")),
                None => parts.push("[non-text content omitted]".to_string()),
            }
        }
    }

    let text = if parts.is_empty() {
        "(empty result)".to_string()
    } else {
        parts.join("\n")
    };
    McpToolOutcome { text, is_error }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_results_join_text_and_placeholder_non_text() {
        let result = json!({
            "content": [
                { "type": "text", "text": "hello" },
                { "type": "image", "data": "xx", "mimeType": "image/png" },
                { "type": "text", "text": "world" },
            ],
        });

        let outcome = parse_tool_result(&result);

        assert!(!outcome.is_error);
        assert_eq!(
            outcome.text,
            "hello\n[non-text content: image omitted]\nworld"
        );
    }

    #[test]
    fn tool_results_surface_is_error_and_empty_content() {
        let error = parse_tool_result(&json!({
            "content": [{ "type": "text", "text": "boom" }],
            "isError": true,
        }));
        assert!(error.is_error);
        assert_eq!(error.text, "boom");

        let empty = parse_tool_result(&json!({ "content": [] }));
        assert_eq!(empty.text, "(empty result)");
        assert!(!empty.is_error);
    }

    #[test]
    fn ring_buffer_keeps_newest_lines_within_budget() {
        let mut ring = RingBuffer::new();
        for index in 0..1000 {
            ring.push_line(&format!("line {index} {}", "x".repeat(100)));
        }

        assert!(ring.bytes <= STDERR_RING_BYTES);
        let tail = ring.tail(4);
        assert!(tail.contains("line 999"), "{tail}");
        assert!(!tail.contains("line 0 "), "{tail}");
    }
}
