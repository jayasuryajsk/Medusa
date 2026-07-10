//! Minimal MCP (Model Context Protocol) client, stdio transport v1.
//!
//! `.medusa/mcp.json` declares servers; the registry spawns them lazily,
//! discovers their tools, and exposes them to the model as namespaced
//! `mcp_<server>_<tool>` function schemas. Dispatch resolves the namespaced
//! name through a full-name map (never string splitting), so server names
//! containing underscores stay unambiguous.

mod client;

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    time::Duration,
};

use color_eyre::eyre::{Result, WrapErr, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use client::McpConnection;
pub use client::McpToolOutcome;

use crate::cancel::CancelToken;

/// Reconnect attempts allowed per server per session after the initial
/// connect; beyond this the server pins Failed until `/mcp restart`.
const MAX_RESTARTS: u32 = 3;
const DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 10;
const DEFAULT_TOOL_TIMEOUT_SECS: u64 = 60;
/// Namespaced tool names longer than this get truncated with a hash suffix.
const MAX_TOOL_NAME_LEN: usize = 64;

/// One server entry from `.medusa/mcp.json`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct McpServerConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// User attestation that this server has no side effects. Only servers
    /// marked `"readOnly": true` are reachable in readonly permission mode
    /// (and advertised to read-only turns).
    #[serde(default, rename = "readOnly")]
    pub read_only: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct McpConfig {
    #[serde(default)]
    servers: BTreeMap<String, McpServerConfig>,
}

/// One discovered tool, cached per server.
#[derive(Debug, Clone)]
pub struct McpToolInfo {
    /// Raw tool name on the server.
    pub name: String,
    /// `mcp_<server>_<tool>` name advertised to the model.
    pub namespaced: String,
    pub description: String,
    pub parameters: Value,
}

/// Connection lifecycle label for `/mcp` and tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpServerStateLabel {
    /// Configured but not started yet.
    Idle,
    Ready,
    /// Was ready; the process died and no call has respawned it yet.
    Disconnected,
    /// Connect/restart in progress on another thread.
    Connecting,
    Failed(String),
}

/// Snapshot of one server for the `/mcp` modal. Built without blocking:
/// a state lock held by an in-flight connect reports `Connecting`.
#[derive(Debug, Clone)]
pub struct McpServerStatus {
    pub name: String,
    pub command_line: String,
    pub state: McpServerStateLabel,
    pub tools: Vec<String>,
    pub stderr_tail: Option<String>,
    pub read_only: bool,
    pub restarts: u32,
}

enum ServerState {
    Idle,
    Ready {
        connection: Arc<McpConnection>,
        tools: Vec<McpToolInfo>,
    },
    Failed {
        error: String,
    },
}

struct McpServer {
    name: String,
    config: McpServerConfig,
    state: Mutex<ServerState>,
    /// Reconnects consumed this session (mutated only under the state lock;
    /// atomic so status snapshots read it without blocking).
    restarts: AtomicU32,
}

/// All configured MCP servers plus the namespaced-tool dispatch map. Created
/// once by the embedder and shared via Arc so ToolRuntime rebuilds re-attach
/// live connections instead of respawning servers.
pub struct McpRegistry {
    workspace: PathBuf,
    servers: BTreeMap<String, McpServer>,
    /// namespaced tool name → (server, raw tool name); grows as servers are
    /// discovered and survives connection deaths so restart retries resolve.
    tool_map: Mutex<HashMap<String, (String, String)>>,
    /// Servers whose *launch* the user approved this session. Spawning a
    /// server runs an arbitrary command, so no process is started until its
    /// name is in this set (Open mode / `/mcp restart` add it directly; other
    /// modes add it only after an explicit approval — see
    /// `ToolRuntime::mcp_tool_schemas`). Repurposes what used to be a
    /// per-server *call* gate, which unlocked every tool after one approval.
    launch_approved: Mutex<BTreeSet<String>>,
    /// Servers whose launch the user declined this session; skipped without
    /// re-prompting until `/mcp restart` clears the decision.
    launch_denied: Mutex<BTreeSet<String>>,
    /// `(server, tool)` pairs the user chose to always-allow this session.
    /// Call approval is scoped per tool: approving one tool never unlocks the
    /// server's other (possibly mutating) tools.
    approved_tools: Mutex<BTreeSet<(String, String)>>,
    /// Set once `shutdown()` runs so a late `ensure_ready` (e.g. an in-flight
    /// worker thread) can never spawn a server that would outlive the process.
    shutting_down: AtomicBool,
}

impl std::fmt::Debug for McpRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpRegistry")
            .field("servers", &self.servers.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl McpRegistry {
    /// Parse `.medusa/mcp.json` under the workspace. A missing file yields an
    /// empty registry; malformed JSON is an error naming the file.
    pub fn load(workspace: impl Into<PathBuf>) -> Result<Arc<Self>> {
        let workspace = workspace.into();
        let path = workspace.join(".medusa").join("mcp.json");
        let config = if path.is_file() {
            let text = fs::read_to_string(&path)
                .wrap_err_with(|| format!("failed to read {}", path.display()))?;
            serde_json::from_str::<McpConfig>(&text)
                .wrap_err_with(|| format!("failed to parse {}", path.display()))?
        } else {
            McpConfig::default()
        };

        let servers = config
            .servers
            .into_iter()
            .filter(|(_, server)| !server.command.trim().is_empty())
            .map(|(name, config)| {
                let server = McpServer {
                    name: name.clone(),
                    config,
                    state: Mutex::new(ServerState::Idle),
                    restarts: AtomicU32::new(0),
                };
                (name, server)
            })
            .collect();

        Ok(Arc::new(Self {
            workspace,
            servers,
            tool_map: Mutex::new(HashMap::new()),
            launch_approved: Mutex::new(BTreeSet::new()),
            launch_denied: Mutex::new(BTreeSet::new()),
            approved_tools: Mutex::new(BTreeSet::new()),
            shutting_down: AtomicBool::new(false),
        }))
    }

    /// A registry with no servers (used when config loading fails and the
    /// embedder wants to continue without MCP).
    pub fn empty() -> Arc<Self> {
        Arc::new(Self {
            workspace: PathBuf::new(),
            servers: BTreeMap::new(),
            tool_map: Mutex::new(HashMap::new()),
            launch_approved: Mutex::new(BTreeSet::new()),
            launch_denied: Mutex::new(BTreeSet::new()),
            approved_tools: Mutex::new(BTreeSet::new()),
            shutting_down: AtomicBool::new(false),
        })
    }

    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    pub fn has_server(&self, name: &str) -> bool {
        self.servers.contains_key(name)
    }

    pub fn server_marked_read_only(&self, name: &str) -> bool {
        self.servers
            .get(name)
            .is_some_and(|server| server.config.read_only)
    }

    /// Configured server names, in stable order.
    pub fn server_names(&self) -> Vec<String> {
        self.servers.keys().cloned().collect()
    }

    /// `command arg arg …` for a configured server (empty for an unknown one).
    /// Surfaced in the launch-approval prompt so the human sees exactly what
    /// would run.
    pub fn server_command_line(&self, name: &str) -> String {
        match self.servers.get(name) {
            Some(server) => std::iter::once(server.config.command.clone())
                .chain(server.config.args.iter().cloned())
                .collect::<Vec<_>>()
                .join(" "),
            None => String::new(),
        }
    }

    /// Whether the user has approved launching (spawning) this server this
    /// session. No server process starts until this is true.
    pub fn server_launch_approved(&self, name: &str) -> bool {
        lock_unpoisoned(&self.launch_approved).contains(name)
    }

    pub fn mark_server_launch_approved(&self, name: &str) {
        lock_unpoisoned(&self.launch_denied).remove(name);
        lock_unpoisoned(&self.launch_approved).insert(name.to_string());
    }

    pub fn mark_server_launch_denied(&self, name: &str) {
        lock_unpoisoned(&self.launch_denied).insert(name.to_string());
    }

    /// Whether a launch approve/deny decision has already been made this
    /// session (so schema builds don't re-prompt every turn).
    pub fn server_launch_decided(&self, name: &str) -> bool {
        self.server_launch_approved(name) || lock_unpoisoned(&self.launch_denied).contains(name)
    }

    /// Whether the user always-allowed this specific `(server, tool)` this
    /// session. Scoped per tool so approving a read tool never unlocks a
    /// mutating one on the same server.
    pub fn tool_approved(&self, server: &str, tool: &str) -> bool {
        lock_unpoisoned(&self.approved_tools).contains(&(server.to_string(), tool.to_string()))
    }

    pub fn mark_tool_approved(&self, server: &str, tool: &str) {
        lock_unpoisoned(&self.approved_tools).insert((server.to_string(), tool.to_string()));
    }

    /// Approve launching every configured server (Open-mode trust of
    /// `.medusa/mcp.json`, and a convenience for transport-level tests).
    pub fn approve_all_launches(&self) {
        let mut approved = lock_unpoisoned(&self.launch_approved);
        for name in self.servers.keys() {
            approved.insert(name.clone());
        }
    }

    /// Resolve a namespaced tool name to `(server, raw tool name)`.
    pub fn lookup(&self, namespaced: &str) -> Option<(String, String)> {
        lock_unpoisoned(&self.tool_map).get(namespaced).cloned()
    }

    /// Namespaced function schemas for every reachable server, refreshing the
    /// cache when a server announced `tools/list_changed`. When
    /// `include_side_effects` is false only servers the user marked
    /// `"readOnly": true` are started and advertised. A server whose launch
    /// has not been approved is skipped (no spawn) — the embedder obtains that
    /// approval before calling this. Blocking — call from a worker thread,
    /// never the UI thread.
    pub fn tool_schemas(&self, include_side_effects: bool, cancel: &CancelToken) -> Vec<Value> {
        let mut schemas = Vec::new();
        for server in self.servers.values() {
            if !include_side_effects && !server.config.read_only {
                continue;
            }
            let Ok(connection) = self.ensure_ready(server, cancel) else {
                continue;
            };
            if connection.take_tools_stale() {
                let _ = self.refresh_tools(server, &connection, cancel);
            }
            let state = lock_unpoisoned(&server.state);
            if let ServerState::Ready { tools, .. } = &*state {
                for tool in tools {
                    schemas.push(json!({
                        "type": "function",
                        "name": tool.namespaced,
                        "description": tool.description,
                        "parameters": tool.parameters,
                    }));
                }
            }
        }
        schemas
    }

    /// Call one tool. A server that dies mid-call is NOT silently respawned
    /// and retried: we cannot tell "request never delivered" from "action
    /// performed, reply lost", so re-sending would run a non-idempotent tool
    /// (charge a card, send an email) twice. Such a crash surfaces an error
    /// for the model to act on; a *later* call may reconnect (via
    /// `ensure_ready`), but never a silent replay of this one.
    pub fn call_tool(
        &self,
        server_name: &str,
        tool: &str,
        arguments: &Value,
        timeout: Duration,
        cancel: &CancelToken,
    ) -> Result<McpToolOutcome> {
        let server = self.server(server_name)?;
        let connection = self.ensure_ready(server, cancel)?;
        match connection.call_tool(tool, arguments, timeout, cancel) {
            Ok(outcome) => Ok(outcome),
            Err(error) if !connection.is_alive() => bail!(
                "MCP server `{server_name}` crashed during `{tool}` and was not retried \
                 (retrying could duplicate side effects); if the action is safe to repeat, \
                 call it again. Underlying error: {error}"
            ),
            Err(error) => Err(error),
        }
    }

    /// Non-blocking snapshot for `/mcp`: a state lock held by an in-flight
    /// connect reports `Connecting` instead of waiting on it.
    pub fn statuses(&self) -> Vec<McpServerStatus> {
        self.servers
            .values()
            .map(|server| {
                let command_line = std::iter::once(server.config.command.clone())
                    .chain(server.config.args.iter().cloned())
                    .collect::<Vec<_>>()
                    .join(" ");
                let (state, tools, stderr_tail) = match server.state.try_lock() {
                    Err(_) => (McpServerStateLabel::Connecting, Vec::new(), None),
                    Ok(state) => match &*state {
                        ServerState::Idle => (McpServerStateLabel::Idle, Vec::new(), None),
                        ServerState::Failed { error } => {
                            (McpServerStateLabel::Failed(error.clone()), Vec::new(), None)
                        }
                        ServerState::Ready { connection, tools } => {
                            let label = if connection.is_alive() {
                                McpServerStateLabel::Ready
                            } else {
                                McpServerStateLabel::Disconnected
                            };
                            let tail = connection.stderr_tail();
                            (
                                label,
                                tools.iter().map(|tool| tool.namespaced.clone()).collect(),
                                (!tail.is_empty()).then_some(tail),
                            )
                        }
                    },
                };
                McpServerStatus {
                    name: server.name.clone(),
                    command_line,
                    state,
                    tools,
                    stderr_tail,
                    read_only: server.config.read_only,
                    restarts: server.restarts.load(Ordering::SeqCst),
                }
            })
            .collect()
    }

    /// Reset the restart budget and reconnect (the `/mcp restart <name>`
    /// escape hatch for servers pinned Failed). Typing this command is an
    /// explicit request to run the server, so it also grants launch approval.
    pub fn restart(&self, name: &str) -> Result<()> {
        let server = self.server(name)?;
        self.mark_server_launch_approved(name);
        {
            let mut state = lock_unpoisoned(&server.state);
            *state = ServerState::Idle;
            server.restarts.store(0, Ordering::SeqCst);
        }
        self.ensure_ready(server, &CancelToken::new()).map(|_| ())
    }

    /// Drop every connection; children get stdin-EOF then a bounded kill. The
    /// shutdown flag also blocks any later spawn so a racing worker thread
    /// cannot resurrect a server after exit.
    pub fn shutdown(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        for server in self.servers.values() {
            let mut state = lock_unpoisoned(&server.state);
            *state = ServerState::Idle;
        }
    }

    fn server(&self, name: &str) -> Result<&McpServer> {
        self.servers
            .get(name)
            .ok_or_else(|| color_eyre::eyre::eyre!("unknown MCP server: {name}"))
    }

    /// Return a live connection, connecting or reconnecting as needed. The
    /// initial connect is free; every reconnect consumes restart budget. A
    /// server is never spawned unless its launch was approved (finding: a
    /// cloned untrusted repo must not auto-run `mcp.json` commands) and never
    /// after `shutdown()`.
    fn ensure_ready(&self, server: &McpServer, cancel: &CancelToken) -> Result<Arc<McpConnection>> {
        let mut state = lock_unpoisoned(&server.state);

        if let ServerState::Ready { connection, .. } = &*state
            && connection.is_alive()
        {
            return Ok(connection.clone());
        }

        // A (re)spawn is imminent: refuse it unless the launch is approved and
        // we are not tearing down. Leave the state untouched (Idle) so a later
        // approval can still connect cleanly.
        if self.shutting_down.load(Ordering::SeqCst) {
            bail!("MCP server `{}`: registry is shutting down", server.name);
        }
        if !self.server_launch_approved(&server.name) {
            bail!(
                "MCP server `{}` was not approved to launch this session",
                server.name
            );
        }

        let restarts = server.restarts.load(Ordering::SeqCst);
        match &*state {
            ServerState::Idle if restarts == 0 => {}
            _ => {
                if restarts >= MAX_RESTARTS {
                    let error = format!(
                        "restart cap reached ({MAX_RESTARTS} per session); run /mcp restart {} to reconnect",
                        server.name
                    );
                    *state = ServerState::Failed {
                        error: error.clone(),
                    };
                    bail!("MCP server `{}`: {error}", server.name);
                }
                server.restarts.store(restarts + 1, Ordering::SeqCst);
            }
        }

        match self.connect(server, cancel) {
            Ok((connection, tools)) => {
                let connection = Arc::new(connection);
                *state = ServerState::Ready {
                    connection: connection.clone(),
                    tools,
                };
                Ok(connection)
            }
            Err(error) => {
                *state = ServerState::Failed {
                    error: error.to_string(),
                };
                Err(error)
            }
        }
    }

    /// Spawn + handshake + tools/list, registering the namespaced names.
    fn connect(
        &self,
        server: &McpServer,
        cancel: &CancelToken,
    ) -> Result<(McpConnection, Vec<McpToolInfo>)> {
        let timeout = connect_timeout();
        let connection = McpConnection::connect(
            &server.name,
            &server.config,
            &self.workspace,
            timeout,
            cancel,
        )?;
        let raw_tools = connection.list_tools(timeout, cancel)?;
        let tools = self.register_tools(&server.name, &raw_tools);
        Ok((connection, tools))
    }

    fn refresh_tools(
        &self,
        server: &McpServer,
        connection: &Arc<McpConnection>,
        cancel: &CancelToken,
    ) -> Result<()> {
        let raw_tools = connection.list_tools(connect_timeout(), cancel)?;
        let tools = self.register_tools(&server.name, &raw_tools);
        let mut state = lock_unpoisoned(&server.state);
        if let ServerState::Ready {
            tools: cached_tools,
            ..
        } = &mut *state
        {
            *cached_tools = tools;
        }
        Ok(())
    }

    /// Namespace raw tool objects and record them in the dispatch map. A
    /// namespaced name already claimed by a different (server, tool) pair is
    /// a collision: that tool is skipped.
    fn register_tools(&self, server: &str, raw_tools: &[Value]) -> Vec<McpToolInfo> {
        let mut map = lock_unpoisoned(&self.tool_map);
        let mut tools = Vec::new();

        for raw in raw_tools {
            let Some(name) = raw.get("name").and_then(Value::as_str) else {
                continue;
            };
            let namespaced = namespaced_tool_name(server, name);
            let target = (server.to_string(), name.to_string());
            match map.get(&namespaced) {
                Some(existing) if *existing != target => continue,
                _ => {}
            }
            map.insert(namespaced.clone(), target);

            let description = raw
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default();
            tools.push(McpToolInfo {
                name: name.to_string(),
                namespaced,
                description: format!("(MCP tool from server `{server}`) {description}"),
                parameters: sanitize_input_schema(raw.get("inputSchema")),
            });
        }

        tools
    }
}

/// A tool's `parameters` must be a JSON Schema *object* — the model request
/// body is rejected wholesale otherwise, bricking every turn in the session.
/// A server that sends `null`, a string, an array, or omits `inputSchema`
/// gets a permissive `{"type":"object"}` substituted so one bad tool cannot
/// poison the request. (`raw.get("inputSchema")` returns `Some(Null)` when the
/// key is present-but-null, which is why the plain `unwrap_or_else` default
/// did not cover it.)
fn sanitize_input_schema(schema: Option<&Value>) -> Value {
    match schema {
        Some(value) if value.is_object() => value.clone(),
        _ => json!({ "type": "object" }),
    }
}

/// Per-call timeout for `tools/call` (`MEDUSA_MCP_TOOL_TIMEOUT_SECS`).
pub fn tool_call_timeout() -> Duration {
    duration_from_env("MEDUSA_MCP_TOOL_TIMEOUT_SECS", DEFAULT_TOOL_TIMEOUT_SECS)
}

/// Connect/handshake budget (`MEDUSA_MCP_CONNECT_TIMEOUT_SECS`).
fn connect_timeout() -> Duration {
    duration_from_env(
        "MEDUSA_MCP_CONNECT_TIMEOUT_SECS",
        DEFAULT_CONNECT_TIMEOUT_SECS,
    )
}

fn duration_from_env(key: &str, default_secs: u64) -> Duration {
    let seconds = std::env::var(key)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default_secs);
    Duration::from_secs(seconds)
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Keep `[a-zA-Z0-9_-]`, map everything else to `_`.
pub(crate) fn sanitize_name_component(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

/// `mcp_<server>_<tool>` capped at 64 chars; longer names truncate and take
/// a hash suffix so distinct tools stay distinct.
fn namespaced_tool_name(server: &str, tool: &str) -> String {
    let name = format!(
        "mcp_{}_{}",
        sanitize_name_component(server),
        sanitize_name_component(tool)
    );
    if name.len() <= MAX_TOOL_NAME_LEN {
        return name;
    }

    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    name.hash(&mut hasher);
    let hash = format!("{:08x}", hasher.finish() & 0xffff_ffff);
    // Sanitized names are pure ASCII, so byte slicing is safe.
    format!("{}_{hash}", &name[..MAX_TOOL_NAME_LEN - 9])
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// Fake stdio MCP server: answers initialize/tools/list, and its tools'
    /// behavior is driven by the call arguments (error, sleep, crash,
    /// garbage-before-reply, wedge, ping_probe, count_file). Env vars select
    /// handshake/pagination quirks. Uses an explicit readline loop so a tool
    /// handler can synchronously read a nested reply (the ping probe).
    const FAKE_SERVER: &str = r#"
import json, os, sys, time

def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

def readmsg():
    line = sys.stdin.readline()
    if line == "":
        return None
    line = line.strip()
    if not line:
        return {}
    return json.loads(line)

PAGINATE = os.environ.get("FAKE_PAGINATE") == "1"
BAD_VERSION = os.environ.get("FAKE_BAD_VERSION") == "1"

TOOLS = [
    {"name": "echo", "description": "Echo text back",
     "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}},
    {"name": "extra", "description": "Second tool", "inputSchema": {"type": "object"}},
]

while True:
    msg = readmsg()
    if msg is None:
        break
    if not msg:
        continue
    method = msg.get("method")
    mid = msg.get("id")
    if method == "initialize":
        version = "1999-01-01" if BAD_VERSION else msg["params"]["protocolVersion"]
        send({"jsonrpc": "2.0", "id": mid, "result": {
            "protocolVersion": version, "capabilities": {"tools": {}},
            "serverInfo": {"name": "fake", "version": "0"}}})
    elif method == "notifications/initialized":
        pass
    elif method == "tools/list":
        cursor = (msg.get("params") or {}).get("cursor")
        if PAGINATE and cursor is None:
            send({"jsonrpc": "2.0", "id": mid,
                  "result": {"tools": [TOOLS[0]], "nextCursor": "page2"}})
        elif PAGINATE:
            send({"jsonrpc": "2.0", "id": mid, "result": {"tools": [TOOLS[1]]}})
        else:
            send({"jsonrpc": "2.0", "id": mid, "result": {"tools": [TOOLS[0]]}})
    elif method == "tools/call":
        args = (msg.get("params") or {}).get("arguments") or {}
        # Observable side effect: append one line per invocation so a test can
        # prove the call ran exactly once (never a silent respawn+replay).
        if args.get("count_file"):
            with open(args["count_file"], "a") as fh:
                fh.write("x\n")
        if args.get("crash"):
            os._exit(1)
        if args.get("wedge"):
            # Never reply; drain stdin until EOF so the caller's request must
            # time out or cancel, then exit cleanly on shutdown.
            while readmsg() is not None:
                pass
            os._exit(0)
        if args.get("sleep"):
            time.sleep(float(args["sleep"]))
        if args.get("garbage"):
            sys.stdout.write("this is not json\n")
            sys.stdout.flush()
        if args.get("ping_probe"):
            # Send a server-initiated ping and report whether the client
            # answered with a success result (spec) rather than an error.
            send({"jsonrpc": "2.0", "id": "srv-ping", "method": "ping"})
            reply = None
            while True:
                nxt = readmsg()
                if nxt is None:
                    break
                if nxt.get("id") == "srv-ping":
                    reply = nxt
                    break
            ok = bool(reply) and ("result" in reply) and ("error" not in reply)
            send({"jsonrpc": "2.0", "id": mid, "result": {"content": [
                {"type": "text", "text": "ping_ok" if ok else "ping_bad"}]}})
        elif args.get("nontext"):
            send({"jsonrpc": "2.0", "id": mid, "result": {"content": [
                {"type": "image", "data": "xx", "mimeType": "image/png"},
                {"type": "text", "text": "with image"}]}})
        elif args.get("error"):
            send({"jsonrpc": "2.0", "id": mid, "result": {
                "content": [{"type": "text", "text": "boom"}], "isError": True}})
        else:
            send({"jsonrpc": "2.0", "id": mid, "result": {"content": [
                {"type": "text", "text": "echo: " + str(args.get("text", ""))}]}})
    elif mid is not None:
        send({"jsonrpc": "2.0", "id": mid,
              "error": {"code": -32601, "message": "unknown method"}})
"#;

    pub(crate) fn temp_workspace() -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let unique = NEXT.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("medusa-mcp-test-{}-{unique}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        path.canonicalize().unwrap()
    }

    /// Write the fake server plus a `.medusa/mcp.json` declaring it under
    /// `server_name`, with extra env vars for behavior flags.
    pub(crate) fn write_fake_server_workspace(
        server_name: &str,
        env: &[(&str, &str)],
        read_only: bool,
    ) -> PathBuf {
        let workspace = temp_workspace();
        let script = workspace.join("fake_mcp_server.py");
        fs::write(&script, FAKE_SERVER).unwrap();

        let env_json = env
            .iter()
            .map(|(key, value)| format!("\"{key}\": \"{value}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let config = format!(
            r#"{{"servers": {{"{server_name}": {{
                "command": "python3",
                "args": ["{}"],
                "env": {{ {env_json} }},
                "readOnly": {read_only}
            }}}}}}"#,
            script.display()
        );
        fs::create_dir_all(workspace.join(".medusa")).unwrap();
        fs::write(workspace.join(".medusa/mcp.json"), config).unwrap();
        workspace
    }

    fn call_timeout() -> Duration {
        Duration::from_secs(10)
    }

    fn no_cancel() -> CancelToken {
        CancelToken::new()
    }

    /// Load the registry and approve every server's launch, so transport-level
    /// tests exercise the wire rather than the launch gate (which is covered
    /// separately). Mirrors Open-mode trust of the workspace config.
    fn loaded(workspace: impl Into<PathBuf>) -> Arc<McpRegistry> {
        let registry = McpRegistry::load(workspace).unwrap();
        registry.approve_all_launches();
        registry
    }

    #[test]
    fn missing_config_yields_empty_registry() {
        let registry = McpRegistry::load(temp_workspace()).unwrap();

        assert!(registry.is_empty());
        assert!(registry.tool_schemas(true, &no_cancel()).is_empty());
        assert!(registry.statuses().is_empty());
    }

    #[test]
    fn unapproved_launch_never_spawns_a_process() {
        // Finding 14: a configured server must not run until its launch is
        // approved. With no approval, tool_schemas/call_tool spawn nothing and
        // the server stays Idle.
        let workspace = write_fake_server_workspace("fake", &[], false);
        let registry = McpRegistry::load(&workspace).unwrap();

        assert!(
            registry.tool_schemas(true, &no_cancel()).is_empty(),
            "no launch approval → no tools advertised"
        );
        assert_eq!(registry.statuses()[0].state, McpServerStateLabel::Idle);

        let error = registry
            .call_tool(
                "fake",
                "echo",
                &json!({"text": "hi"}),
                call_timeout(),
                &no_cancel(),
            )
            .unwrap_err();
        assert!(
            error.to_string().contains("not approved to launch"),
            "{error}"
        );
        assert_eq!(
            registry.statuses()[0].state,
            McpServerStateLabel::Idle,
            "a denied launch leaves the server unspawned"
        );

        // After approval the same paths connect and work.
        registry.mark_server_launch_approved("fake");
        assert_eq!(registry.tool_schemas(true, &no_cancel()).len(), 1);
        assert_eq!(registry.statuses()[0].state, McpServerStateLabel::Ready);
    }

    #[test]
    fn malformed_config_errors_with_the_file_path() {
        let workspace = temp_workspace();
        fs::create_dir_all(workspace.join(".medusa")).unwrap();
        fs::write(workspace.join(".medusa/mcp.json"), "{not json").unwrap();

        let error = McpRegistry::load(&workspace).unwrap_err();

        assert!(error.to_string().contains("mcp.json"), "{error}");
    }

    #[test]
    fn config_parses_servers_with_args_env_and_read_only() {
        let workspace = temp_workspace();
        fs::create_dir_all(workspace.join(".medusa")).unwrap();
        fs::write(
            workspace.join(".medusa/mcp.json"),
            r#"{"servers": {"docs": {"command": "python3", "args": ["-u", "server.py"],
                "env": {"TOKEN": "x"}, "readOnly": true}}}"#,
        )
        .unwrap();

        let registry = McpRegistry::load(&workspace).unwrap();

        assert!(registry.has_server("docs"));
        assert!(registry.server_marked_read_only("docs"));
        let status = &registry.statuses()[0];
        assert_eq!(status.command_line, "python3 -u server.py");
        assert_eq!(status.state, McpServerStateLabel::Idle);
    }

    #[test]
    fn handshake_discovers_namespaced_tool_schemas() {
        let workspace = write_fake_server_workspace("fake", &[], false);
        let registry = loaded(&workspace);

        let schemas = registry.tool_schemas(true, &no_cancel());

        assert_eq!(schemas.len(), 1);
        assert_eq!(schemas[0]["type"], "function");
        assert_eq!(schemas[0]["name"], "mcp_fake_echo");
        assert!(
            schemas[0]["description"]
                .as_str()
                .unwrap()
                .contains("Echo text back")
        );
        assert_eq!(schemas[0]["parameters"]["type"], "object");
        assert_eq!(
            registry.lookup("mcp_fake_echo"),
            Some(("fake".to_string(), "echo".to_string()))
        );

        let status = &registry.statuses()[0];
        assert_eq!(status.state, McpServerStateLabel::Ready);
        assert_eq!(status.tools, vec!["mcp_fake_echo".to_string()]);
    }

    #[test]
    fn tools_list_follows_next_cursor_pagination() {
        let workspace = write_fake_server_workspace("fake", &[("FAKE_PAGINATE", "1")], false);
        let registry = loaded(&workspace);

        let schemas = registry.tool_schemas(true, &no_cancel());

        let names: Vec<&str> = schemas
            .iter()
            .filter_map(|schema| schema["name"].as_str())
            .collect();
        assert_eq!(names, vec!["mcp_fake_echo", "mcp_fake_extra"]);
    }

    #[test]
    fn unsupported_protocol_version_fails_the_connection() {
        let workspace = write_fake_server_workspace("fake", &[("FAKE_BAD_VERSION", "1")], false);
        let registry = loaded(&workspace);

        assert!(registry.tool_schemas(true, &no_cancel()).is_empty());
        let status = &registry.statuses()[0];
        match &status.state {
            McpServerStateLabel::Failed(error) => {
                assert!(error.contains("1999-01-01"), "{error}");
            }
            other => panic!("expected Failed state, got {other:?}"),
        }
    }

    #[test]
    fn read_only_gating_hides_side_effect_servers() {
        let workspace = write_fake_server_workspace("fake", &[], false);
        let registry = loaded(&workspace);

        // Not marked readOnly: hidden from read-only turns, visible otherwise.
        assert!(registry.tool_schemas(false, &no_cancel()).is_empty());
        assert_eq!(registry.tool_schemas(true, &no_cancel()).len(), 1);

        let workspace = write_fake_server_workspace("safe", &[], true);
        let registry = loaded(&workspace);
        assert_eq!(registry.tool_schemas(false, &no_cancel()).len(), 1);
    }

    #[test]
    fn call_tool_round_trips_text_and_is_error() {
        let workspace = write_fake_server_workspace("fake", &[], false);
        let registry = loaded(&workspace);

        let ok = registry
            .call_tool(
                "fake",
                "echo",
                &json!({"text": "hi"}),
                call_timeout(),
                &no_cancel(),
            )
            .unwrap();
        assert_eq!(ok.text, "echo: hi");
        assert!(!ok.is_error);

        let error = registry
            .call_tool(
                "fake",
                "echo",
                &json!({"error": true}),
                call_timeout(),
                &no_cancel(),
            )
            .unwrap();
        assert!(error.is_error);
        assert_eq!(error.text, "boom");

        let nontext = registry
            .call_tool(
                "fake",
                "echo",
                &json!({"nontext": true}),
                call_timeout(),
                &no_cancel(),
            )
            .unwrap();
        assert!(nontext.text.contains("[non-text content: image omitted]"));
        assert!(nontext.text.contains("with image"));
    }

    #[test]
    fn server_ping_is_answered_with_a_success_result() {
        // Finding 17: a server-initiated `ping` must get an empty result, not
        // -32601 (keepalive servers drop the connection otherwise). The fake
        // server reports "ping_ok" only if the client answered with a result.
        let workspace = write_fake_server_workspace("fake", &[], false);
        let registry = loaded(&workspace);

        let outcome = registry
            .call_tool(
                "fake",
                "echo",
                &json!({"ping_probe": true}),
                call_timeout(),
                &no_cancel(),
            )
            .unwrap();

        assert_eq!(
            outcome.text, "ping_ok",
            "client must answer ping with a result"
        );
        assert!(!outcome.is_error);
    }

    #[test]
    fn call_bails_promptly_when_the_cancel_token_flips_mid_call() {
        // Finding 6: an MCP call parked waiting on a wedged server must abort
        // within a poll interval of the cancel token flipping, not after the
        // full tool timeout.
        let workspace = write_fake_server_workspace("fake", &[], false);
        let registry = loaded(&workspace);

        let cancel = CancelToken::new();
        let flipper = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            flipper.cancel();
        });

        let started = std::time::Instant::now();
        let error = registry
            .call_tool(
                "fake",
                "echo",
                &json!({"wedge": true}),
                // A 60s timeout would be the pre-fix wait; cancel must win.
                Duration::from_secs(60),
                &cancel,
            )
            .unwrap_err();

        assert!(
            crate::cancel::error_is_cancellation(&error),
            "expected cancellation, got: {error}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "cancel must interrupt the wedged call promptly (took {:?})",
            started.elapsed()
        );
    }

    #[test]
    fn timeout_discards_late_reply_and_connection_stays_usable() {
        let workspace = write_fake_server_workspace("fake", &[], false);
        let registry = loaded(&workspace);

        let started = std::time::Instant::now();
        let error = registry
            .call_tool(
                "fake",
                "echo",
                &json!({"sleep": 1.0, "text": "slow"}),
                Duration::from_millis(150),
                &no_cancel(),
            )
            .unwrap_err();
        assert!(error.to_string().contains("timed out"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(5));

        // Wait out the sleeping reply so it arrives as a late (discarded)
        // response, then verify the next call still correlates correctly.
        std::thread::sleep(Duration::from_millis(1_200));
        let ok = registry
            .call_tool(
                "fake",
                "echo",
                &json!({"text": "after"}),
                call_timeout(),
                &no_cancel(),
            )
            .unwrap();
        assert_eq!(ok.text, "echo: after");
        assert_eq!(
            registry.statuses()[0].restarts,
            0,
            "timeout must not respawn"
        );
    }

    #[test]
    fn garbage_stdout_lines_are_skipped() {
        let workspace = write_fake_server_workspace("fake", &[], false);
        let registry = loaded(&workspace);

        let ok = registry
            .call_tool(
                "fake",
                "echo",
                &json!({"garbage": true, "text": "still works"}),
                call_timeout(),
                &no_cancel(),
            )
            .unwrap();

        assert_eq!(ok.text, "echo: still works");
    }

    #[test]
    fn mid_call_crash_is_not_silently_retried() {
        // Finding 16: a server that performs its side effect then dies before
        // replying must NOT be respawned and re-invoked — that would run a
        // non-idempotent tool twice. The side-effect file must hold exactly
        // one line, and the error must say the call was not retried.
        let workspace = write_fake_server_workspace("fake", &[], false);
        let registry = loaded(&workspace);
        let side_effect = workspace.join("side_effect.log");
        let side_effect_str = side_effect.to_string_lossy().to_string();

        let error = registry
            .call_tool(
                "fake",
                "echo",
                &json!({"count_file": side_effect_str, "crash": true}),
                call_timeout(),
                &no_cancel(),
            )
            .unwrap_err();
        assert!(error.to_string().contains("not retried"), "{error}");

        let lines = fs::read_to_string(&side_effect).unwrap();
        assert_eq!(
            lines.lines().count(),
            1,
            "the crashing call must execute exactly once (no silent replay)"
        );
        assert_eq!(
            registry.statuses()[0].restarts,
            0,
            "a mid-call crash must not respawn the server for this call"
        );

        // A *later* call may reconnect (respawn happens for the next call, not
        // as a replay of the crashed one).
        let ok = registry
            .call_tool(
                "fake",
                "echo",
                &json!({"text": "back"}),
                call_timeout(),
                &no_cancel(),
            )
            .unwrap();
        assert_eq!(ok.text, "echo: back");
        assert_eq!(registry.statuses()[0].restarts, 1);
    }

    #[test]
    fn restart_cap_pins_failed_then_recovers() {
        let workspace = write_fake_server_workspace("fake", &[], false);
        let registry = loaded(&workspace);

        // Each crash kills the server without a respawn+retry; the following
        // normal call reconnects, consuming one restart. Exhaust the budget.
        for _ in 0..MAX_RESTARTS {
            assert!(
                registry
                    .call_tool(
                        "fake",
                        "echo",
                        &json!({"crash": true}),
                        call_timeout(),
                        &no_cancel()
                    )
                    .is_err()
            );
            let ok = registry
                .call_tool(
                    "fake",
                    "echo",
                    &json!({"text": "back"}),
                    call_timeout(),
                    &no_cancel(),
                )
                .unwrap();
            assert_eq!(ok.text, "echo: back");
        }

        // Budget spent. One more crash, then the reconnect attempt trips the
        // cap and pins Failed with the /mcp restart hint.
        assert!(
            registry
                .call_tool(
                    "fake",
                    "echo",
                    &json!({"crash": true}),
                    call_timeout(),
                    &no_cancel()
                )
                .is_err()
        );
        let error = registry
            .call_tool(
                "fake",
                "echo",
                &json!({"text": "nope"}),
                call_timeout(),
                &no_cancel(),
            )
            .unwrap_err();
        assert!(error.to_string().contains("/mcp restart"), "{error}");
        assert!(matches!(
            registry.statuses()[0].state,
            McpServerStateLabel::Failed(_)
        ));

        // /mcp restart resets the budget and reconnects.
        registry.restart("fake").unwrap();
        let ok = registry
            .call_tool(
                "fake",
                "echo",
                &json!({"text": "again"}),
                call_timeout(),
                &no_cancel(),
            )
            .unwrap();
        assert_eq!(ok.text, "echo: again");
        assert_eq!(registry.statuses()[0].state, McpServerStateLabel::Ready);
    }

    #[test]
    fn shutdown_reaps_the_child_process() {
        let workspace = write_fake_server_workspace("fake", &[], false);
        let registry = loaded(&workspace);
        registry.tool_schemas(true, &no_cancel());

        let pid = {
            let server = registry.servers.get("fake").unwrap();
            let state = server.state.lock().unwrap();
            match &*state {
                ServerState::Ready { connection, .. } => connection.pid() as i32,
                _ => panic!("server should be ready"),
            }
        };

        registry.shutdown();

        // The fake server exits on stdin EOF; give the OS a moment to reap.
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            let alive = unsafe { libc::kill(pid, 0) } == 0;
            if !alive {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "MCP child survived shutdown"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    #[test]
    fn shutdown_blocks_any_later_spawn() {
        // Finding 19: once shutdown has run, a racing call (e.g. an in-flight
        // worker thread) must not spawn a server that would outlive the
        // process. The launch is approved yet ensure_ready still refuses.
        let workspace = write_fake_server_workspace("fake", &[], false);
        let registry = loaded(&workspace);

        registry.shutdown();

        let error = registry
            .call_tool(
                "fake",
                "echo",
                &json!({"text": "hi"}),
                call_timeout(),
                &no_cancel(),
            )
            .unwrap_err();
        assert!(error.to_string().contains("shutting down"), "{error}");
        assert_eq!(
            registry.statuses()[0].state,
            McpServerStateLabel::Idle,
            "no process may start after shutdown"
        );
    }

    #[test]
    fn namespaced_names_sanitize_cap_and_stay_distinct() {
        assert_eq!(namespaced_tool_name("fake", "echo"), "mcp_fake_echo");
        assert_eq!(
            namespaced_tool_name("my.server", "read file!"),
            "mcp_my_server_read_file_"
        );

        let long_a = namespaced_tool_name("server", &"a".repeat(120));
        let long_b = namespaced_tool_name("server", &format!("{}b", "a".repeat(119)));
        assert_eq!(long_a.len(), MAX_TOOL_NAME_LEN);
        assert_eq!(long_b.len(), MAX_TOOL_NAME_LEN);
        assert_ne!(long_a, long_b, "hash suffix keeps long names distinct");
    }

    #[test]
    fn lookup_round_trips_server_names_containing_underscores() {
        let workspace = write_fake_server_workspace("my_server", &[], false);
        let registry = loaded(&workspace);

        let schemas = registry.tool_schemas(true, &no_cancel());

        assert_eq!(schemas[0]["name"], "mcp_my_server_echo");
        assert_eq!(
            registry.lookup("mcp_my_server_echo"),
            Some(("my_server".to_string(), "echo".to_string()))
        );
    }

    #[test]
    fn colliding_namespaced_names_keep_the_first_registration() {
        let registry = McpRegistry::empty();
        let tool = json!({"name": "run", "description": "", "inputSchema": {"type": "object"}});

        // `a.b` and `a_b` both sanitize to `a_b`: second registration loses.
        let first = registry.register_tools("a.b", std::slice::from_ref(&tool));
        let second = registry.register_tools("a_b", std::slice::from_ref(&tool));

        assert_eq!(first.len(), 1);
        assert!(second.is_empty(), "collision must be skipped");
        assert_eq!(
            registry.lookup("mcp_a_b_run"),
            Some(("a.b".to_string(), "run".to_string()))
        );
    }

    #[test]
    fn malformed_input_schema_is_replaced_with_a_permissive_object() {
        // Finding 15: a tool whose inputSchema is null / non-object / missing
        // must not emit `"parameters": null` (or a non-object) into the model
        // request body — one bad tool would 400 every turn. Each gets a
        // permissive {"type":"object"} substituted so good tools survive.
        let registry = McpRegistry::empty();
        let raw = vec![
            json!({ "name": "null_schema", "description": "", "inputSchema": null }),
            json!({ "name": "string_schema", "description": "", "inputSchema": "nope" }),
            json!({ "name": "array_schema", "description": "", "inputSchema": [1, 2, 3] }),
            json!({ "name": "missing_schema", "description": "" }),
            json!({
                "name": "good_schema", "description": "",
                "inputSchema": { "type": "object", "properties": { "x": { "type": "string" } } }
            }),
        ];

        let tools = registry.register_tools("srv", &raw);

        assert_eq!(tools.len(), 5);
        for tool in &tools {
            assert!(
                tool.parameters.is_object(),
                "{} parameters must be an object, got {:?}",
                tool.name,
                tool.parameters
            );
            assert_eq!(
                tool.parameters["type"], "object",
                "{} must advertise an object schema",
                tool.name
            );
        }
        // The valid schema is preserved verbatim (properties intact).
        let good = tools.iter().find(|t| t.name == "good_schema").unwrap();
        assert_eq!(good.parameters["properties"]["x"]["type"], "string");
    }

    #[test]
    fn sanitize_input_schema_substitutes_only_non_objects() {
        assert_eq!(sanitize_input_schema(None), json!({ "type": "object" }));
        assert_eq!(
            sanitize_input_schema(Some(&Value::Null)),
            json!({ "type": "object" })
        );
        assert_eq!(
            sanitize_input_schema(Some(&json!("string"))),
            json!({ "type": "object" })
        );
        assert_eq!(
            sanitize_input_schema(Some(&json!([1, 2]))),
            json!({ "type": "object" })
        );
        let object = json!({ "type": "object", "required": ["a"] });
        assert_eq!(sanitize_input_schema(Some(&object)), object);
    }
}
