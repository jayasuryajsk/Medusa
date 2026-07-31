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

    pub fn terminal_exec(&self, request: TerminalExecRequest) -> Result<TerminalExecResult> {
        self.terminal_exec_gated(request, false)
    }

    /// `preapproved` skips the interactive gate (never the hard denies); used
    /// by explore probes that already passed the read-only probe allowlist.
    pub(crate) fn terminal_exec_gated(
        &self,
        request: TerminalExecRequest,
        preapproved: bool,
    ) -> Result<TerminalExecResult> {
        let check = self.permissions.evaluate_terminal_command(&request.command);
        if request.unsandboxed {
            if self.permissions.effective_mode() == PermissionMode::Readonly {
                bail!(
                    "terminal.exec sandbox escalation refused: readonly mode never runs commands unsandboxed"
                );
            }
            // Escaping the sandbox always takes a fresh human decision, even
            // for commands that would otherwise auto-run. Hard denies stay
            // hard.
            let check = match check {
                PermissionCheck::Deny(reason) => PermissionCheck::Deny(reason),
                PermissionCheck::Allow | PermissionCheck::NeedsApproval => {
                    PermissionCheck::NeedsApproval
                }
            };
            self.authorize(check, || ApprovalRequest {
                tool: ApprovalTool::TerminalExec,
                command: Some(request.command.clone()),
                paths: Vec::new(),
                background: request.background,
                sandbox_escalation: true,
            })?;
        } else if preapproved && check == PermissionCheck::NeedsApproval {
            // probe allowlist already vetted this as read-only
        } else {
            self.authorize(check, || ApprovalRequest {
                tool: ApprovalTool::TerminalExec,
                command: Some(request.command.clone()),
                paths: Vec::new(),
                background: request.background,
                sandbox_escalation: false,
            })?;
        }
        let cwd = self.resolve_workspace_path(request.cwd.as_deref())?;
        // `preapproved` is exactly the explore-probe path: those read-only
        // probes always sandbox strictly (network denied) when available.
        let strict = preapproved;

        if request.background {
            let (command, sandboxed) =
                self.build_shell_command(&request.command, &cwd, strict, request.unsandboxed)?;
            let child = crate::proc::spawn_command(command)
                .wrap_err_with(|| format!("failed to start command: {}", request.command))?;

            let pid = child.id();
            let id = background_job_id(pid, &request.command, &cwd);
            let command = request.command.clone();
            let event_cwd = cwd.clone();
            if let Some(sender) = self.background_events.clone() {
                let _ = sender.send(BackgroundJobEvent::Started {
                    id: id.clone(),
                    pid,
                    command: command.clone(),
                    cwd: event_cwd.clone(),
                });
                let finish_id = id.clone();
                let fail_id = id.clone();
                thread::spawn(move || {
                    let event = match crate::proc::wait_for_child(child) {
                        Ok(output) => BackgroundJobEvent::Finished {
                            id: finish_id,
                            pid,
                            command,
                            cwd: event_cwd,
                            code: output.code,
                            stdout: output.stdout,
                            stderr: output.stderr,
                        },
                        Err(error) => BackgroundJobEvent::Failed {
                            id: fail_id,
                            pid,
                            command,
                            cwd: event_cwd,
                            error: error.to_string(),
                        },
                    };
                    let _ = sender.send(event);
                });
            } else {
                thread::spawn(move || {
                    let _ = crate::proc::wait_for_child(child);
                });
            }

            return Ok(TerminalExecResult {
                command: request.command,
                cwd,
                code: Some(0),
                stdout: String::new(),
                stderr: String::new(),
                background: true,
                pid: Some(pid),
                job_id: Some(id),
                sandboxed,
            });
        }

        // Foreground runs are cancellable (background jobs deliberately are
        // not: they outlive the turn by design). Never spawn after cancel.
        self.cancel.bail_if_cancelled()?;

        let (command, sandboxed) =
            self.build_shell_command(&request.command, &cwd, strict, request.unsandboxed)?;
        let outcome = crate::proc::run_command(command, None, &self.cancel)
            .wrap_err_with(|| format!("failed to run command: {}", request.command))?;
        if outcome.cancelled {
            bail!("cancelled: interrupted by user");
        }

        Ok(TerminalExecResult {
            command: request.command,
            cwd,
            code: outcome.code,
            stdout: outcome.stdout,
            stderr: outcome.stderr,
            background: false,
            pid: None,
            job_id: None,
            sandboxed,
        })
    }

    /// Build `$SHELL -lc <command>` for both terminal_exec paths, wrapped in
    /// the platform sandbox when the policy (or a strict explore probe) asks
    /// for it. Sandbox-required commands fail closed when the backend is
    /// unavailable; only an explicit, approved escalation gets a plain shell.
    fn build_shell_command(
        &self,
        command_text: &str,
        cwd: &Path,
        strict: bool,
        unsandboxed: bool,
    ) -> Result<(Command, bool)> {
        let shell = std::env::var_os("SHELL").unwrap_or_else(|| OsStr::new("sh").to_os_string());
        if !unsandboxed && (self.sandbox.should_sandbox() || strict) {
            match crate::sandbox::sandbox_availability() {
                SandboxAvailability::Available => {
                    let spec = self.sandbox.spec(&self.workspace, strict);
                    return Ok((
                        crate::sandbox::wrap_command(&spec, &shell, command_text, cwd),
                        true,
                    ));
                }
                SandboxAvailability::Broken(reason) => {
                    bail!(
                        "sandbox-required command blocked: {reason}. Install/fix the platform sandbox, switch to open permissions, or request an explicit unsandboxed run for user approval"
                    );
                }
                SandboxAvailability::UnsupportedPlatform => {
                    bail!(
                        "sandbox-required command blocked: this platform has no supported Medusa sandbox. Switch to open permissions or request an explicit unsandboxed run for user approval"
                    );
                }
            }
        }

        let mut command = Command::new(shell);
        command.arg("-lc").arg(command_text).current_dir(cwd);
        Ok((command, false))
    }

    pub fn file_read(&self, request: FileReadRequest) -> Result<FileReadResult> {
        if request.paths.is_empty() {
            bail!("file_read.paths cannot be empty");
        }

        let start_line = request.start_line.unwrap_or(1).max(1);
        let end_line = request.end_line.unwrap_or(start_line + 240).max(start_line);
        let mut files = Vec::new();

        for path in request.paths {
            let resolved = self.resolve_workspace_path(Some(&path))?;
            if !resolved.is_file() {
                bail!("file_read path is not a file: {}", path.display());
            }

            let content = fs::read_to_string(&resolved)
                .wrap_err_with(|| format!("failed to read {}", resolved.display()))?;
            let lines = content.lines().collect::<Vec<_>>();
            let total_lines = lines.len();
            let start_index = start_line.saturating_sub(1).min(total_lines);
            let end_index = end_line.min(total_lines);
            let mut selected = lines[start_index..end_index]
                .iter()
                .enumerate()
                .map(|(offset, line)| NumberedLine {
                    number: start_line + offset,
                    text: (*line).to_string(),
                })
                .collect::<Vec<_>>();
            let truncated = selected.len() > 260;
            selected.truncate(260);

            files.push(ReadFile {
                path: self.workspace_relative(&resolved),
                start_line,
                end_line: if selected.is_empty() {
                    start_line
                } else {
                    selected.last().map_or(start_line, |line| line.number)
                },
                total_lines,
                truncated,
                lines: selected,
            });
        }

        Ok(FileReadResult { files })
    }

    pub fn file_search(&self, request: FileSearchRequest) -> Result<FileSearchResult> {
        let query = request.query.trim();
        if query.is_empty() {
            bail!("file_search.query cannot be empty");
        }

        let root = self.resolve_workspace_path(request.path.as_deref())?;
        let max_results = request.max_results.unwrap_or(80).clamp(1, 500);
        let case_sensitive = request.case_sensitive.unwrap_or(true);
        let matcher = SearchMatcher::new(query, case_sensitive);
        let include = request
            .include
            .as_deref()
            .map(compile_include_glob)
            .transpose()?;
        let mut matches = Vec::new();
        let mut searched_files = 0usize;

        for file in self.walk_files(&root, request.depth.unwrap_or(8).clamp(0, 16))? {
            if matches.len() >= max_results {
                break;
            }
            if let Some(include) = &include
                && !include.is_match(self.workspace_relative(&file))
            {
                continue;
            }
            if file.metadata().map(|meta| meta.len()).unwrap_or(0) > 2_000_000 {
                continue;
            }
            let Ok(content) = fs::read_to_string(&file) else {
                continue;
            };
            searched_files += 1;
            for (line_index, line) in content.lines().enumerate() {
                if matcher.is_match(line) {
                    matches.push(SearchMatch {
                        path: self.workspace_relative(&file),
                        line: line_index + 1,
                        text: line.trim_end().chars().take(240).collect(),
                    });
                    if matches.len() >= max_results {
                        break;
                    }
                }
            }
        }

        let truncated = matches.len() >= max_results;
        Ok(FileSearchResult {
            query: query.to_string(),
            regex: matcher.is_regex(),
            matches,
            searched_files,
            truncated,
        })
    }

    pub fn file_glob(&self, request: FileGlobRequest) -> Result<FileGlobResult> {
        let pattern = request.pattern.trim();
        if pattern.is_empty() {
            bail!("file_glob.pattern cannot be empty");
        }

        let root = self.resolve_workspace_path(request.path.as_deref())?;
        let max_results = request.max_results.unwrap_or(120).clamp(1, 500);
        let glob = compile_include_glob(pattern)?;

        let mut matched = Vec::new();
        for file in self.walk_files(&root, 16)? {
            let relative = self.workspace_relative(&file);
            if glob.is_match(&relative) {
                let modified = file
                    .metadata()
                    .and_then(|meta| meta.modified())
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                matched.push((relative, modified));
            }
        }

        matched.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let truncated = matched.len() > max_results;
        matched.truncate(max_results);

        Ok(FileGlobResult {
            pattern: pattern.to_string(),
            root: self.workspace_relative(&root),
            paths: matched.into_iter().map(|(path, _)| path).collect(),
            truncated,
        })
    }

    pub fn fs_list(&self, request: FsListRequest) -> Result<FsListResult> {
        let root = self.resolve_workspace_path(request.path.as_deref())?;
        let max_depth = request.depth.unwrap_or(2).clamp(0, 8);
        let max_entries = request.max_entries.unwrap_or(120).clamp(1, 500);
        let mut entries = Vec::new();
        let mut truncated = false;

        self.collect_list_entries(
            &root,
            0,
            max_depth,
            max_entries,
            &mut entries,
            &mut truncated,
        )?;

        Ok(FsListResult {
            root: self.workspace_relative(&root),
            entries,
            truncated,
        })
    }

    pub fn file_patch(&self, request: FilePatchRequest) -> Result<FilePatchResult> {
        let cwd = self.resolve_workspace_path(request.cwd.as_deref())?;
        let diff = normalize_patch(&request.diff);
        let diff = diff.as_str();

        if diff.trim().is_empty() {
            bail!("patch is empty");
        }

        let changed_files = extract_patch_paths(diff)?;
        if changed_files.is_empty() {
            bail!("patch does not contain any file paths");
        }

        let workspace_changed_files = self.workspace_relative_patch_paths(&cwd, &changed_files)?;
        self.authorize(
            self.permissions
                .evaluate_patch_paths(&workspace_changed_files),
            || ApprovalRequest {
                tool: ApprovalTool::FilePatch,
                command: None,
                paths: workspace_changed_files.clone(),
                background: false,
                sandbox_escalation: false,
            },
        )?;

        // After approval (denied ops never create checkpoints), before either
        // apply path writes anything.
        self.capture_checkpoint(&workspace_changed_files)?;

        if is_codex_patch(diff) {
            apply_codex_patch(&cwd, diff)?;
            return Ok(FilePatchResult {
                changed_files: workspace_changed_files,
            });
        }

        let mut recount = false;
        if let Err(error) = run_git_apply(&cwd, diff, true, false) {
            let first_error = error.to_string();
            recount = true;
            run_git_apply(&cwd, diff, true, true)
                .wrap_err_with(|| format!("{first_error}; retry with --recount also failed"))?;
        }
        run_git_apply(&cwd, diff, false, recount)?;

        Ok(FilePatchResult {
            changed_files: workspace_changed_files,
        })
    }

    pub fn file_edit(&self, request: FileEditRequest) -> Result<FileEditResult> {
        let path = request.path.to_string_lossy().to_string();
        validate_relative_path(&path)?;
        self.authorize(
            self.permissions
                .evaluate_patch_paths(std::slice::from_ref(&path)),
            || ApprovalRequest {
                tool: ApprovalTool::FileEdit,
                command: None,
                paths: vec![path.clone()],
                background: false,
                sandbox_escalation: false,
            },
        )?;

        if request.old_string == request.new_string {
            bail!("old_string and new_string must differ");
        }

        let candidate = self.workspace.join(&request.path);
        if !candidate.exists() {
            if !request.old_string.is_empty() {
                bail!("file_edit target does not exist: {}", path);
            }
            if let Some(parent) = candidate.parent() {
                let existing_parent = parent
                    .ancestors()
                    .find(|ancestor| ancestor.exists())
                    .unwrap_or(&self.workspace);
                let canonical_parent = existing_parent
                    .canonicalize()
                    .wrap_err_with(|| format!("failed to resolve {}", existing_parent.display()))?;
                if !canonical_parent.starts_with(&self.workspace) {
                    bail!("path escapes workspace: {}", candidate.display());
                }
                fs::create_dir_all(parent)
                    .wrap_err_with(|| format!("failed to create {}", parent.display()))?;
            }
            // Capture ONLY after confirming the resolved parent is inside the
            // workspace — capturing earlier would snapshot (and copy into
            // .medusa) a file reached through an out-of-workspace symlink,
            // poisoning the manifest with a host path. Records `absent`.
            self.capture_checkpoint(std::slice::from_ref(&path))?;
            atomic_write(&candidate, request.new_string)
                .wrap_err_with(|| format!("failed to write {}", candidate.display()))?;
            return Ok(FileEditResult {
                path,
                replacements: 1,
            });
        }

        if request.old_string.is_empty() {
            bail!("old_string cannot be empty for existing files");
        }

        let resolved = self.resolve_workspace_path(Some(&request.path))?;
        if !resolved.is_file() {
            bail!("file_edit path is not a file: {}", path);
        }

        // resolve_workspace_path canonicalized and confirmed `resolved` is
        // inside the workspace (rejecting out-of-workspace symlink targets), so
        // it is now safe to snapshot the pre-image before the write.
        self.capture_checkpoint(std::slice::from_ref(&path))?;

        let content = fs::read_to_string(&resolved)
            .wrap_err_with(|| format!("failed to read {}", resolved.display()))?;
        let matches = content
            .match_indices(&request.old_string)
            .collect::<Vec<_>>();
        if matches.is_empty() {
            let normalized_old = request.old_string.replace("\r\n", "\n");
            let normalized_content = content.replace("\r\n", "\n");
            if normalized_old != request.old_string
                && normalized_content.matches(&normalized_old).count() > 0
            {
                bail!(
                    "old_string was not found exactly; it appears to match only after line-ending normalization. Re-read the file and retry with exact text."
                );
            }
            bail!("old_string was not found exactly once in {}", path);
        }
        if matches.len() > 1 && !request.replace_all {
            bail!(
                "old_string matched {} times in {}; provide more context or set replace_all=true",
                matches.len(),
                path
            );
        }

        let new_content = if request.replace_all {
            content.replace(&request.old_string, &request.new_string)
        } else {
            content.replacen(&request.old_string, &request.new_string, 1)
        };
        atomic_write(&resolved, new_content)
            .wrap_err_with(|| format!("failed to write {}", resolved.display()))?;

        Ok(FileEditResult {
            path,
            replacements: if request.replace_all {
                matches.len()
            } else {
                1
            },
        })
    }

    pub fn task_update(&self, request: TaskUpdateRequest) -> Result<TaskUpdateResult> {
        let status = request.status.trim();
        if status.is_empty() {
            bail!("status cannot be empty");
        }

        Ok(TaskUpdateResult {
            status: status.chars().take(160).collect(),
        })
    }

    pub fn plan_update(&self, request: PlanUpdateRequest) -> Result<PlanUpdateResult> {
        if request.items.is_empty() {
            bail!("plan items cannot be empty");
        }

        let mut active_count = 0usize;
        let mut items = Vec::new();
        for item in request.items.into_iter().take(24) {
            let text = item.text.trim();
            if text.is_empty() {
                continue;
            }
            let status = normalize_plan_status(&item.status)
                .ok_or_else(|| color_eyre::eyre::eyre!("unknown plan status: {}", item.status))?;
            if status == "active" {
                active_count += 1;
            }
            let evidence = item
                .evidence
                .into_iter()
                .map(|value| value.trim().chars().take(180).collect::<String>())
                .filter(|value| !value.is_empty())
                .take(6)
                .collect::<Vec<_>>();
            items.push(PlanUpdateItem {
                text: text.chars().take(220).collect(),
                status: status.to_string(),
                evidence,
            });
        }

        if items.is_empty() {
            bail!("plan items cannot all be empty");
        }
        if active_count > 1 {
            bail!("at most one plan item can be active");
        }

        Ok(PlanUpdateResult {
            summary: request
                .summary
                .unwrap_or_default()
                .trim()
                .chars()
                .take(180)
                .collect(),
            items,
        })
    }

    pub fn question(&self, request: QuestionRequest) -> Result<QuestionResult> {
        let question = request.question.trim();
        if question.is_empty() {
            bail!("question cannot be empty");
        }

        Ok(QuestionResult {
            question: question.chars().take(600).collect(),
        })
    }

    pub fn decision_request(&self, request: DecisionRequest) -> Result<DecisionResult> {
        if request.questions.is_empty() {
            bail!("decision_request.questions cannot be empty");
        }

        let mut seen_ids = BTreeSet::new();
        let mut questions = Vec::new();
        for (index, question) in request.questions.into_iter().take(8).enumerate() {
            let prompt = question.prompt.trim();
            if prompt.is_empty() {
                continue;
            }

            let kind = normalize_decision_kind(&question.kind);
            let options = question
                .options
                .into_iter()
                .map(|option| option.trim().chars().take(120).collect::<String>())
                .filter(|option| !option.is_empty())
                .take(8)
                .collect::<Vec<_>>();
            if kind == "choice" && options.is_empty() {
                bail!(
                    "decision_request.questions[{index}].options is required for choice questions"
                );
            }

            let base_id = sanitize_decision_id(&question.id)
                .filter(|id| !id.is_empty())
                .unwrap_or_else(|| format!("q{}", index + 1));
            let mut id = base_id.clone();
            let mut suffix = 2usize;
            while !seen_ids.insert(id.clone()) {
                id = format!("{base_id}_{suffix}");
                suffix += 1;
            }

            let recommended = question
                .recommended
                .map(|value| value.trim().chars().take(120).collect::<String>())
                .filter(|value| !value.is_empty());

            questions.push(DecisionQuestion {
                id,
                prompt: prompt.chars().take(280).collect(),
                kind: kind.to_string(),
                options,
                recommended,
                required: question.required,
            });
        }

        if questions.is_empty() {
            bail!("decision_request.questions cannot all be empty");
        }

        let assumptions = request
            .assumptions
            .into_iter()
            .map(|assumption| assumption.trim().chars().take(220).collect::<String>())
            .filter(|assumption| !assumption.is_empty())
            .take(6)
            .collect::<Vec<_>>();

        Ok(DecisionResult {
            title: request
                .title
                .unwrap_or_else(|| "Planning decision".to_string())
                .trim()
                .chars()
                .take(120)
                .collect(),
            reason: request
                .reason
                .unwrap_or_default()
                .trim()
                .chars()
                .take(280)
                .collect(),
            questions,
            assumptions,
        })
    }

    pub fn explore_batch(&self, request: ExploreBatchRequest) -> Result<ExploreBatchResult> {
        if request.probes.is_empty() {
            bail!("explore_batch.probes cannot be empty");
        }

        let goal = request.goal.trim().chars().take(240).collect::<String>();
        let probes = request.probes.into_iter().take(12).collect::<Vec<_>>();
        let total = probes.len();
        let batch_started = Instant::now();
        let (sender, receiver) = mpsc::channel();

        for (index, probe) in probes.into_iter().enumerate() {
            let sender = sender.clone();
            let tools = self.clone();
            thread::spawn(move || {
                let result = run_explore_probe(&tools, index, probe);
                let _ = sender.send((index, result));
            });
        }
        drop(sender);

        let mut results = vec![None; total];
        for (index, result) in receiver {
            if let Some(slot) = results.get_mut(index) {
                *slot = Some(result);
            }
        }

        let probes = results
            .into_iter()
            .enumerate()
            .map(|(index, result)| {
                result.unwrap_or_else(|| ExploreProbeResult {
                    index,
                    kind: "unknown".to_string(),
                    label: format!("probe {}", index + 1),
                    failed: true,
                    output: "probe worker ended without returning a result".to_string(),
                    elapsed_ms: 0,
                })
            })
            .collect::<Vec<_>>();
        let failed = probes.iter().filter(|probe| probe.failed).count();

        Ok(ExploreBatchResult {
            goal,
            probes,
            failed,
            elapsed_ms: batch_started.elapsed().as_millis(),
        })
    }

    pub fn read_patch_file(&self, path: &str) -> Result<String> {
        let path = self.resolve_workspace_path(Some(Path::new(path)))?;
        fs::read_to_string(&path)
            .wrap_err_with(|| format!("failed to read patch file: {}", path.display()))
    }

    fn resolve_workspace_path(&self, path: Option<&Path>) -> Result<PathBuf> {
        let Some(path) = path else {
            return Ok(self.workspace.clone());
        };

        let candidate = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.workspace.join(path)
        };

        let canonical = candidate
            .canonicalize()
            .wrap_err_with(|| format!("path does not exist: {}", candidate.display()))?;

        if !canonical.starts_with(&self.workspace) {
            bail!(
                "path escapes workspace: {} is outside {}",
                canonical.display(),
                self.workspace.display()
            );
        }

        Ok(canonical)
    }

    fn workspace_relative(&self, path: &Path) -> String {
        path.strip_prefix(&self.workspace)
            .unwrap_or(path)
            .to_string_lossy()
            .trim_start_matches('/')
            .to_string()
            .if_empty(".")
    }

    fn workspace_relative_patch_paths(&self, cwd: &Path, paths: &[String]) -> Result<Vec<String>> {
        let cwd_relative = cwd.strip_prefix(&self.workspace).wrap_err_with(|| {
            format!(
                "patch cwd {} is outside workspace {}",
                cwd.display(),
                self.workspace.display()
            )
        })?;
        let mut normalized = BTreeSet::new();

        for path in paths {
            validate_relative_path(path)?;
            let workspace_path = cwd_relative.join(path);
            let normalized_path = normalize_workspace_relative_path(&workspace_path)?;
            // Lexical validation alone is not enough: `home/.gitconfig` where
            // `home` is a symlink to `~` has only Normal components yet its
            // real location is outside the workspace. Reject such paths BEFORE
            // capture snapshots (and copies into .medusa) or git apply writes
            // through the link.
            self.ensure_patch_path_within_workspace(&normalized_path)?;
            normalized.insert(normalized_path);
        }

        Ok(normalized.into_iter().collect())
    }

    /// Refuse a workspace-relative patch path whose real filesystem location
    /// escapes the workspace through a symlink component, or whose final
    /// component is itself a symlink (writing/snapshotting would dereference
    /// it outside the workspace). New paths whose parent does not exist yet
    /// resolve their deepest existing ancestor, which for a legitimate patch
    /// is the workspace root.
    fn ensure_patch_path_within_workspace(&self, rel_path: &str) -> Result<()> {
        let target = self.workspace.join(rel_path);
        let mut probe = target.parent().unwrap_or(&self.workspace).to_path_buf();
        let real_parent = loop {
            match probe.canonicalize() {
                Ok(canonical) => break canonical,
                Err(_) => match probe.parent() {
                    Some(up) if up != probe => probe = up.to_path_buf(),
                    _ => bail!("cannot resolve parent of patch path: {rel_path}"),
                },
            }
        };
        if !real_parent.starts_with(&self.workspace) {
            bail!("patch path escapes workspace through a symlink: {rel_path}");
        }
        if let Ok(meta) = fs::symlink_metadata(&target)
            && meta.file_type().is_symlink()
        {
            bail!("patch path is a symlink; refusing to follow it: {rel_path}");
        }
        Ok(())
    }

    fn walk_files(&self, root: &Path, max_depth: usize) -> Result<Vec<PathBuf>> {
        let mut files = Vec::new();
        Self::collect_files(root, 0, max_depth, &mut files)?;
        files.sort();
        Ok(files)
    }

    fn collect_files(
        path: &Path,
        depth: usize,
        max_depth: usize,
        files: &mut Vec<PathBuf>,
    ) -> Result<()> {
        if path.is_file() {
            files.push(path.to_path_buf());
            return Ok(());
        }
        if depth > max_depth || !path.is_dir() {
            return Ok(());
        }

        for entry in sorted_read_dir(path)? {
            let entry_path = entry.path();
            if entry_path.is_dir() {
                if should_skip_dir(&entry_path) {
                    continue;
                }
                Self::collect_files(&entry_path, depth + 1, max_depth, files)?;
            } else if entry_path.is_file() {
                files.push(entry_path);
            }
        }

        Ok(())
    }

    fn collect_list_entries(
        &self,
        path: &Path,
        depth: usize,
        max_depth: usize,
        max_entries: usize,
        entries: &mut Vec<FsEntry>,
        truncated: &mut bool,
    ) -> Result<()> {
        if entries.len() >= max_entries {
            *truncated = true;
            return Ok(());
        }

        if depth > 0 && path.is_dir() && should_skip_dir(path) {
            return Ok(());
        }

        if path != self.workspace || depth > 0 {
            entries.push(FsEntry {
                path: self.workspace_relative(path),
                kind: if path.is_dir() { "dir" } else { "file" }.to_string(),
                depth,
            });
        }

        if depth >= max_depth || !path.is_dir() {
            return Ok(());
        }

        for entry in sorted_read_dir(path)? {
            if entries.len() >= max_entries {
                *truncated = true;
                break;
            }
            self.collect_list_entries(
                &entry.path(),
                depth + 1,
                max_depth,
                max_entries,
                entries,
                truncated,
            )?;
        }

        Ok(())
    }
}
