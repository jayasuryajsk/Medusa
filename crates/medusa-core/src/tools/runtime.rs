use std::{
    collections::BTreeSet,
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc,
        mpsc::{self, Sender},
    },
    thread,
    time::Instant,
};

use color_eyre::eyre::{Result, WrapErr, bail};

use crate::cancel::CancelToken;
use crate::checkpoint::CheckpointRecorder;
use crate::hooks::HookRuntime;
use crate::mcp::{McpRegistry, McpToolOutcome};
use crate::permissions::{PermissionCheck, PermissionMode, PermissionPolicy};
use crate::persistence::atomic_write;
use crate::sandbox::{SandboxAvailability, SandboxPolicy};
use crate::semantic::SemanticRuntime;
use crate::skills::SkillRegistry;

use super::approval::{
    ApprovalDecision, ApprovalHandler, ApprovalRequest, ApprovalTool, Authorization,
};
use super::support::{
    IfEmpty, apply_codex_patch, extract_patch_paths, is_codex_patch, mcp_arguments_preview,
    normalize_decision_kind, normalize_patch, normalize_plan_status,
    normalize_workspace_relative_path, run_explore_probe, run_git_apply, sanitize_decision_id,
    should_skip_dir, sorted_read_dir, validate_relative_path,
};
use super::types::*;

#[derive(Clone)]
pub struct ToolRuntime {
    workspace: PathBuf,
    hooks: HookRuntime,
    permissions: PermissionPolicy,
    skills: SkillRegistry,
    sandbox: SandboxPolicy,
    background_events: Option<Sender<BackgroundJobEvent>>,
    approval_handler: Option<ApprovalHandler>,
    checkpoints: Option<CheckpointRecorder>,
    /// Shared MCP server registry. Arc-shared so every ToolRuntime clone and
    /// periodic rebuild re-attaches the same live connections instead of
    /// respawning servers.
    mcp: Option<Arc<McpRegistry>>,
    /// Turn-level cancellation flag; the default token never cancels, so
    /// runtimes built outside a cancellable turn behave exactly as before.
    cancel: CancelToken,
    semantic: Arc<SemanticRuntime>,
}

impl std::fmt::Debug for ToolRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolRuntime")
            .field("workspace", &self.workspace)
            .field("approval_handler", &self.approval_handler.is_some())
            .finish_non_exhaustive()
    }
}

impl ToolRuntime {
    pub fn new(workspace: impl Into<PathBuf>) -> Result<Self> {
        let workspace = workspace.into();
        let workspace = workspace
            .canonicalize()
            .wrap_err_with(|| format!("workspace does not exist: {}", workspace.display()))?;
        let hooks = HookRuntime::load(&workspace)?;
        let permissions = PermissionPolicy::load(&workspace)?;
        let skills = SkillRegistry::load(&workspace)?;
        let sandbox = SandboxPolicy::load(&permissions);

        Ok(Self {
            semantic: Arc::new(SemanticRuntime::new(workspace.clone())),
            workspace,
            hooks,
            permissions,
            skills,
            sandbox,
            background_events: None,
            approval_handler: None,
            checkpoints: None,
            mcp: None,
            cancel: CancelToken::default(),
        })
    }

    /// Replace the resolved sandbox policy (tests and callers with an
    /// out-of-band stance).
    pub fn with_sandbox(mut self, sandbox: SandboxPolicy) -> Self {
        self.sandbox = sandbox;
        self
    }

    /// Override the permission mode for this runtime without persisting it.
    /// Non-interactive callers use this so their CLI flag governs both tool
    /// authorization and the derived sandbox stance.
    pub fn with_permission_mode(mut self, mode: PermissionMode) -> Self {
        self.permissions = self.permissions.with_mode_override(mode);
        self.sandbox = SandboxPolicy::load(&self.permissions);
        self
    }

    pub fn sandbox_policy(&self) -> &SandboxPolicy {
        &self.sandbox
    }

    pub fn with_background_events(mut self, sender: Sender<BackgroundJobEvent>) -> Self {
        self.background_events = Some(sender);
        self
    }

    pub fn with_approval_handler(mut self, handler: ApprovalHandler) -> Self {
        self.approval_handler = Some(handler);
        self
    }

    /// Attach a per-turn checkpoint recorder. Mutating file tools capture
    /// pre-images through it right after approval and before any write.
    pub fn with_checkpoint_recorder(mut self, recorder: CheckpointRecorder) -> Self {
        self.checkpoints = Some(recorder);
        self
    }

    /// Attach the embedder-owned MCP registry so namespaced `mcp_*` tools
    /// resolve and execute through its live server connections.
    pub fn with_mcp(mut self, registry: Arc<McpRegistry>) -> Self {
        self.mcp = Some(registry);
        self
    }

    /// Attach the turn's cancellation token. Every clone path — parallel
    /// read-only threads, explore probes, workflow subagents — inherits it.
    pub fn with_cancel_token(mut self, token: CancelToken) -> Self {
        self.cancel = token;
        self
    }

    pub fn cancel_token(&self) -> &CancelToken {
        &self.cancel
    }

    /// Whether a checkpoint recorder is attached, so mutating file tools
    /// capture pre-images. Lets embedders assert the turn/workflow wiring is
    /// present rather than silently dropped.
    pub fn has_checkpoint_recorder(&self) -> bool {
        self.checkpoints.is_some()
    }

    /// Snapshot pre-images for the given workspace-relative paths. No-op
    /// without a recorder; a capture failure fails the calling mutation
    /// (fail-closed) — a silently missing snapshot is worse than a blocked
    /// edit.
    fn capture_checkpoint(&self, paths: &[String]) -> Result<()> {
        let Some(recorder) = &self.checkpoints else {
            return Ok(());
        };
        recorder
            .capture(paths)
            .wrap_err("checkpoint capture failed; aborting the file mutation")
    }

    /// Resolve a three-state permission check, pausing on the approval
    /// handler when user consent is required. Without a handler (headless,
    /// tests), approval-needing operations are auto-denied. Returns how the
    /// grant was obtained so callers can persist session scope only on an
    /// explicit always-allow.
    fn authorize(
        &self,
        check: PermissionCheck,
        request: impl FnOnce() -> ApprovalRequest,
    ) -> Result<Authorization> {
        match check {
            PermissionCheck::Allow => Ok(Authorization::Allowed),
            PermissionCheck::Deny(reason) => bail!("{reason}"),
            PermissionCheck::NeedsApproval => {
                // A cancelled turn must never park the worker on (or
                // re-prompt) the approval UI; bail before consulting the
                // handler.
                self.cancel.bail_if_cancelled()?;
                let request = request();
                let what = request.tool.label();
                let Some(handler) = &self.approval_handler else {
                    bail!("{what} requires approval; auto-denied (no approver attached)");
                };
                match handler(request) {
                    ApprovalDecision::AllowOnce => Ok(Authorization::GrantedOnce),
                    ApprovalDecision::AlwaysAllow => Ok(Authorization::GrantedAlways),
                    ApprovalDecision::Deny => bail!("{what} denied by user"),
                }
            }
        }
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    pub fn semantic_search(&self, request: SemanticSearchRequest) -> Result<SemanticSearchResult> {
        self.cancel.bail_if_cancelled()?;
        self.semantic.search(request, &self.cancel)
    }

    pub fn hooks(&self) -> &HookRuntime {
        &self.hooks
    }

    pub fn skills(&self) -> &SkillRegistry {
        &self.skills
    }

    pub fn mcp(&self) -> Option<&Arc<McpRegistry>> {
        self.mcp.as_ref()
    }

    /// Namespaced MCP function schemas to merge into the model's tools array.
    /// Empty without a registry. When `include_side_effects` is false only
    /// servers marked `"readOnly": true` are advertised. Spawning a server
    /// runs an arbitrary `.medusa/mcp.json` command, so the *first* time each
    /// server would start this session, non-Open modes require a human
    /// launch approval; only then is it spawned and its tools discovered.
    /// Blocking on first use (lazy connect) — call from a worker thread.
    pub fn mcp_tool_schemas(&self, include_side_effects: bool) -> Vec<serde_json::Value> {
        let Some(registry) = &self.mcp else {
            return Vec::new();
        };
        for server in registry.server_names() {
            if !include_side_effects && !registry.server_marked_read_only(&server) {
                continue;
            }
            // A prior approve/deny decision (or a cancelled prompt) means we
            // don't re-ask on every turn's schema build.
            if registry.server_launch_decided(&server) {
                continue;
            }
            let command_line = registry.server_command_line(&server);
            match self.authorize_mcp_launch(&server, &command_line) {
                Ok(()) => registry.mark_server_launch_approved(&server),
                Err(error) if crate::cancel::error_is_cancellation(&error) => {
                    // Turn cancelled mid-prompt: leave undecided so the user
                    // can approve next turn.
                }
                Err(_) => registry.mark_server_launch_denied(&server),
            }
        }
        registry.tool_schemas(include_side_effects, &self.cancel)
    }

    /// Approve launching (spawning) an MCP server. Open mode trusts the
    /// workspace config; every confined mode routes the launch — which runs an
    /// arbitrary command — through the approval gate so a freshly-cloned
    /// untrusted repo can never auto-execute `mcp.json` commands.
    fn authorize_mcp_launch(&self, server: &str, command_line: &str) -> Result<()> {
        let check = match self.permissions.effective_mode() {
            PermissionMode::Open => PermissionCheck::Allow,
            PermissionMode::Guarded | PermissionMode::Ask | PermissionMode::Readonly => {
                PermissionCheck::NeedsApproval
            }
        };
        self.authorize(check, || ApprovalRequest {
            tool: ApprovalTool::McpServerLaunch,
            command: Some(format!("launch MCP server `{server}`: {command_line}")),
            paths: Vec::new(),
            background: false,
            sandbox_escalation: false,
        })
        .map(|_| ())
    }

    /// Ensure a server's launch is approved (prompting once per session in
    /// non-Open modes) before any call path spawns it.
    fn ensure_mcp_server_launch_approved(
        &self,
        registry: &Arc<McpRegistry>,
        server: &str,
    ) -> Result<()> {
        if registry.server_launch_approved(server) {
            return Ok(());
        }
        let command_line = registry.server_command_line(server);
        self.authorize_mcp_launch(server, &command_line)?;
        registry.mark_server_launch_approved(server);
        Ok(())
    }

    /// Resolve a namespaced `mcp_*` tool name to `(server, tool)` via the
    /// registry's full-name map (never string splitting).
    pub fn mcp_lookup(&self, namespaced: &str) -> Option<(String, String)> {
        self.mcp
            .as_ref()
            .and_then(|registry| registry.lookup(namespaced))
    }

    /// Execute one MCP tool call through the permission gate. MCP servers
    /// run outside the workspace boundary and may have side effects, so:
    /// Open allows; Readonly refuses servers the user did not explicitly mark
    /// `"readOnly": true`; Guarded/Ask require (a) a launch approval before
    /// the server process is spawned and (b) a per-`(server, tool)` call
    /// approval — approving one tool never unlocks the server's other tools,
    /// and "allow once" authorizes exactly this call.
    pub fn mcp_call(
        &self,
        namespaced: &str,
        arguments: &serde_json::Value,
    ) -> Result<McpToolOutcome> {
        let Some(registry) = &self.mcp else {
            bail!("MCP tool {namespaced} is unavailable: no MCP registry attached");
        };
        let Some((server, tool)) = registry.lookup(namespaced) else {
            bail!("unknown MCP tool: {namespaced}");
        };

        let check = match self.permissions.effective_mode() {
            PermissionMode::Open => PermissionCheck::Allow,
            PermissionMode::Readonly => {
                if registry.server_marked_read_only(&server) {
                    PermissionCheck::Allow
                } else {
                    PermissionCheck::Deny(format!(
                        "mcp.call denied by readonly permissions: server `{server}` is not marked \"readOnly\": true in .medusa/mcp.json"
                    ))
                }
            }
            PermissionMode::Guarded | PermissionMode::Ask => {
                if registry.tool_approved(&server, &tool) {
                    PermissionCheck::Allow
                } else {
                    PermissionCheck::NeedsApproval
                }
            }
        };

        // Gate the server *launch* (arbitrary command execution) before the
        // per-call gate, so a fresh repo can't spawn a process on first use.
        self.ensure_mcp_server_launch_approved(registry, &server)?;

        let grant = self.authorize(check, || ApprovalRequest {
            tool: ApprovalTool::McpTool,
            command: Some(format!(
                "{server}:{tool} {}",
                mcp_arguments_preview(arguments)
            )),
            paths: Vec::new(),
            background: false,
            sandbox_escalation: false,
        })?;
        // Persist the grant only when the user chose "always allow", and only
        // for this exact tool: "allow once" (or a policy allow) authorizes
        // just this call.
        if grant.is_always() {
            registry.mark_tool_approved(&server, &tool);
        }

        self.cancel.bail_if_cancelled()?;
        registry.call_tool(
            &server,
            &tool,
            arguments,
            crate::mcp::tool_call_timeout(),
            &self.cancel,
        )
    }

    /// Outbound network egress permission for `web_fetch`/`web_search`. Open
    /// trusts the workspace and auto-allows; every confined mode
    /// (Guarded/Ask/Readonly) routes the request through the approval gate so
    /// the sandbox's network-denial cannot be bypassed by an in-process fetch.
    fn web_egress_check(&self) -> PermissionCheck {
        match self.permissions.effective_mode() {
            PermissionMode::Open => PermissionCheck::Allow,
            PermissionMode::Guarded | PermissionMode::Ask | PermissionMode::Readonly => {
                PermissionCheck::NeedsApproval
            }
        }
    }

    /// Fetch a public http(s) URL through the egress gate. Unlike a sandbox
    /// escalation, an always-allow decision is honoured (no `sandbox_escalation`).
    pub fn web_fetch(&self, request: crate::web::WebFetchRequest) -> Result<String> {
        self.authorize(self.web_egress_check(), || ApprovalRequest {
            tool: ApprovalTool::WebFetch,
            command: Some(request.url.clone()),
            paths: Vec::new(),
            background: false,
            sandbox_escalation: false,
        })?;
        self.cancel.bail_if_cancelled()?;
        crate::web::web_fetch(&request)
    }

    /// Run a web search through the egress gate (see [`Self::web_fetch`]).
    pub fn web_search(&self, request: crate::web::WebSearchRequest) -> Result<String> {
        self.authorize(self.web_egress_check(), || ApprovalRequest {
            tool: ApprovalTool::WebSearch,
            command: Some(request.query.clone()),
            paths: Vec::new(),
            background: false,
            sandbox_escalation: false,
        })?;
        self.cancel.bail_if_cancelled()?;
        crate::web::web_search(&request)
    }
}

mod explore;
mod files;
mod paths;
mod planning;
mod terminal;
