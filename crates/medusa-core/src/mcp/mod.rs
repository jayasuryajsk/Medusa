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
pub(crate) mod tests;
