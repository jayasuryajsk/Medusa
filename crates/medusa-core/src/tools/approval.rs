use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalTool {
    TerminalExec,
    FileEdit,
    FilePatch,
    /// A namespaced MCP tool call; `ApprovalRequest.command` carries
    /// `server:tool <args preview>`.
    McpTool,
    /// Launching (spawning) an MCP server, which runs an arbitrary command
    /// from `.medusa/mcp.json`; `ApprovalRequest.command` carries
    /// `server: <command line>`.
    McpServerLaunch,
    /// Outbound `web_fetch`; `ApprovalRequest.command` carries the URL.
    WebFetch,
    /// Outbound `web_search`; `ApprovalRequest.command` carries the query.
    WebSearch,
}

impl ApprovalTool {
    pub fn label(self) -> &'static str {
        match self {
            Self::TerminalExec => "terminal.exec",
            Self::FileEdit => "file.edit",
            Self::FilePatch => "file.patch",
            Self::McpTool => "mcp.call",
            Self::McpServerLaunch => "mcp.launch",
            Self::WebFetch => "web.fetch",
            Self::WebSearch => "web.search",
        }
    }
}

/// One paused tool call awaiting a user decision.
#[derive(Debug, Clone)]
pub struct ApprovalRequest {
    pub tool: ApprovalTool,
    pub command: Option<String>,
    pub paths: Vec<String>,
    pub background: bool,
    /// The command asked to escape the Seatbelt sandbox (`"sandbox": false`).
    /// Escalations always require a fresh human decision: stored grants must
    /// never auto-approve them and always-allow must not be offered.
    pub sandbox_escalation: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalDecision {
    AllowOnce,
    AlwaysAllow,
    Deny,
}

/// How an [`ToolRuntime::authorize`] check resolved to "allowed". Only
/// `GrantedAlways` should persist a session-scoped grant; `GrantedOnce`
/// authorizes exactly the one operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Authorization {
    /// Policy allowed it outright (no prompt shown).
    Allowed,
    /// User pressed "allow once".
    GrantedOnce,
    /// User pressed "always allow".
    GrantedAlways,
}

impl Authorization {
    /// Whether the user explicitly asked to remember this for the session.
    pub(crate) fn is_always(self) -> bool {
        matches!(self, Authorization::GrantedAlways)
    }
}

/// Blocks the calling worker thread until a decision arrives. Shared across
/// every ToolRuntime clone (worker threads, explore probes, workflow
/// subagents) via Arc.
pub type ApprovalHandler = Arc<dyn Fn(ApprovalRequest) -> ApprovalDecision + Send + Sync>;
