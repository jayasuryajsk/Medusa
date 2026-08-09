use super::*;

impl App {
    pub(super) fn submit_input(&mut self) {
        let task = self.input.trim().to_string();
        if task.is_empty() && self.pending_attachments.is_empty() {
            if self.pending_decision().is_some() {
                self.accept_decision_enter();
            } else if !self.is_working()
                && !self.has_active_workflows()
                && !self.queued_turns.is_empty()
            {
                // Prompts kept from a cancelled turn ([21]) run on an explicit
                // empty submit — never silently auto-launched.
                self.start_next_queued_turn();
            } else {
                self.status_line = "Type a task first.".to_string();
            }
            return;
        }

        // `# <note>` is quick memory: record it in AGENTS.md instead of
        // sending a model turn (works even mid-turn or with a decision
        // pending — a note is never an answer).
        if let Some(note) = task.strip_prefix("# ") {
            let note = note.trim().to_string();
            self.record_quick_memory(&note);
            return;
        }

        if self.pending_decision().is_some()
            && self.pending_attachments.is_empty()
            && !task.starts_with('/')
        {
            self.accept_decision_enter();
            return;
        }

        let attachments = std::mem::take(&mut self.pending_attachments);
        self.attachment_previews.clear();
        self.input.clear();
        self.input_cursor = 0;
        self.refresh_mention_state();
        if attachments.is_empty() && self.run_local_tool_command(&task) {
            self.persist_session();
            self.scroll_chat_to_bottom();
            return;
        }

        if self.is_working() || self.has_active_workflows() || self.is_compacting() {
            if !attachments.is_empty() {
                self.pending_attachments = attachments;
                for attachment in self.pending_attachments.clone() {
                    self.cache_attachment_preview(&attachment);
                }
                self.status_line = "finish current turn before sending images".to_string();
                self.toast("Image turns cannot be queued yet", ToastKind::Warning);
                return;
            }
            self.queued_turns.push_back(task.clone());
            self.status_line = format!(
                "queued: {}{}",
                truncate(&task, 48),
                queue_count_suffix(self.queued_turns.len())
            );
            return;
        }

        self.transcript
            .push(TranscriptItem::Message(ChatMessage::user_with_attachments(
                task.clone(),
                attachments,
            )));
        self.touch_transcript();
        self.persist_session();
        self.scroll_chat_to_bottom();
        self.start_model_turn(&task);
    }

    /// Quick memory: append the note under `## Notes` in AGENTS.md and leave
    /// a muted transcript line. Nothing is sent to the model now — project
    /// instructions are reloaded from AGENTS.md at the start of every turn,
    /// so the note applies from the next turn automatically.
    pub(super) fn record_quick_memory(&mut self, note: &str) {
        self.input.clear();
        self.input_cursor = 0;
        if note.is_empty() {
            self.status_line = "empty note".to_string();
            self.toast("Nothing to note", ToastKind::Warning);
            return;
        }

        match append_quick_memory(self.tools.workspace(), note) {
            Ok(()) => {
                self.transcript
                    .push(TranscriptItem::Message(ChatMessage::system(format!(
                        "noted in AGENTS.md: {note} (applies from next turn)"
                    ))));
                self.touch_transcript();
                self.persist_session();
                self.scroll_chat_to_bottom();
                self.status_line = "noted in AGENTS.md".to_string();
                self.toast("noted in AGENTS.md", ToastKind::Success);
            }
            Err(error) => {
                self.status_line = format!("note failed: {error}");
                self.toast("Note failed", ToastKind::Error);
            }
        }
    }

    pub(super) fn run_local_tool_command(&mut self, task: &str) -> bool {
        if task.starts_with('/')
            && self.is_compacting()
            && !matches!(
                task,
                "/compact" | "/context" | "/cost" | "/help" | "/commands" | "/jobs"
            )
        {
            self.status_line = "finish compaction before changing session state".to_string();
            self.toast(
                "Wait for compaction to finish, or press Esc to cancel it",
                ToastKind::Warning,
            );
            return true;
        }

        if task == "/help" || task == "/commands" {
            self.active_modal = Some(if task == "/help" {
                Modal::Help
            } else {
                Modal::Commands
            });
            self.status_line = if task == "/help" {
                "help opened".to_string()
            } else {
                "commands opened".to_string()
            };
            return true;
        }

        if task == "/jobs" {
            self.active_modal = Some(Modal::Jobs);
            self.status_line = format!("{} background jobs", self.background_jobs.len());
            return true;
        }

        if let Some(id) = task.strip_prefix("/kill ") {
            self.kill_background_job(id.trim());
            return true;
        }

        if let Some(id) = task.strip_prefix("/tail ") {
            self.tail_background_job(id.trim());
            return true;
        }

        if let Some(id) = task.strip_prefix("/restart ") {
            self.restart_background_job(id.trim());
            return true;
        }

        if task == "/plan" {
            self.toggle_plan_mode();
            return true;
        }

        if task == "/reload" {
            self.request_reload();
            return true;
        }

        if task == "/workflows" {
            self.active_modal = Some(Modal::Workflows);
            self.status_line = "workflows opened".to_string();
            return true;
        }

        if task == "/workflow" {
            let scripts = WorkflowScript::list(self.tools.workspace());
            let script_lines = if scripts.is_empty() {
                "No saved scripts yet. Add JavaScript workflows under .medusa/workflows/<name>.js"
                    .to_string()
            } else {
                format!(
                    "Saved scripts:\n{}",
                    scripts
                        .iter()
                        .map(|name| format!("  {name}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                )
            };
            self.transcript
                .push(TranscriptItem::Message(ChatMessage::system(format!(
                    "usage: /workflow <script-name> [args]  — run a saved JS workflow script\n       /workflow <task>               — have the model author and run a task-specific JS workflow\n\n{script_lines}",
                ))));
            self.touch_transcript();
            self.status_line = "workflow needs a task or script".to_string();
            self.toast("Workflow task required", ToastKind::Warning);
            return true;
        }

        if let Some(workflow_task) = task.strip_prefix("/workflow ") {
            self.start_workflow_request(workflow_task);
            return true;
        }

        if task == "/sessions" {
            self.active_modal = Some(Modal::Sessions);
            self.status_line = "sessions opened".to_string();
            return true;
        }

        if task == "/tree" {
            self.active_modal = Some(Modal::SessionTree);
            self.status_line = "session tree opened".to_string();
            return true;
        }

        if task == "/resume" {
            self.transcript
                .push(TranscriptItem::Message(ChatMessage::system(
                    "usage: /resume <session>\n\nUse /sessions to inspect saved session names.",
                )));
            self.touch_transcript();
            self.status_line = "resume needs a session".to_string();
            return true;
        }

        if let Some(session_id) = task.strip_prefix("/resume ") {
            self.resume_session(session_id.trim());
            return true;
        }

        if task == "/fork" || task == "/branch" {
            self.fork_session();
            return true;
        }

        if task == "/rewind" {
            self.open_rewind_modal();
            return true;
        }

        if task == "/edit" {
            self.open_edit_message_modal();
            return true;
        }

        if task == "/review" {
            self.run_review_command();
            return true;
        }

        if task == "/cost" {
            self.active_modal = Some(Modal::Cost);
            self.status_line = "token usage opened".to_string();
            return true;
        }

        if task == "/context" {
            self.context_report = Some(self.build_context_report());
            self.active_modal = Some(Modal::Context);
            self.status_line = "context breakdown opened".to_string();
            return true;
        }

        if task == "/compact" {
            self.run_compact_command();
            return true;
        }

        if task == "/clear" {
            self.transcript.clear();
            self.touch_transcript();
            self.selected_tool = None;
            self.context_engine.reset();
            self.last_compaction = None;
            self.toast("Session cleared", ToastKind::Warning);
            self.status_line = "cleared".to_string();
            return true;
        }

        if task == "/settings" {
            self.open_settings_modal();
            return true;
        }

        if task == "/model" {
            self.open_models_modal();
            return true;
        }

        if let Some(model) = task.strip_prefix("/model ") {
            self.set_model_name(model);
            return true;
        }

        if task == "/reasoning" || task == "/effort" || task == "/think" {
            self.open_reasoning_modal();
            return true;
        }

        if let Some(effort) = task
            .strip_prefix("/reasoning ")
            .or_else(|| task.strip_prefix("/effort "))
            .or_else(|| task.strip_prefix("/think "))
        {
            self.set_reasoning_effort(effort);
            return true;
        }

        if task == "/permissions" || task == "/permission" {
            self.open_permissions_modal();
            return true;
        }

        if let Some(mode) = task
            .strip_prefix("/permissions ")
            .or_else(|| task.strip_prefix("/permission "))
        {
            if let Some(mode) = PermissionMode::from_name(mode) {
                self.set_permission_mode(mode);
            } else {
                let available = PermissionMode::all()
                    .iter()
                    .map(|mode| mode.name())
                    .collect::<Vec<_>>()
                    .join(", ");
                self.transcript
                    .push(TranscriptItem::Message(ChatMessage::system(format!(
                        "unknown permission mode: {mode}\n\nAvailable modes: {available}"
                    ))));
                self.touch_transcript();
                self.status_line = "unknown permission mode".to_string();
                self.toast("Unknown permission mode", ToastKind::Error);
            }
            return true;
        }

        if task == "/theme" {
            self.open_themes_modal();
            return true;
        }

        if let Some(theme_name) = task.strip_prefix("/theme ") {
            let theme_name = theme_name.trim();
            if matches!(theme_name, "next" | "+") {
                self.cycle_theme(1);
            } else if matches!(theme_name, "prev" | "previous" | "-") {
                self.cycle_theme(-1);
            } else if let Some(theme) = ThemeKind::from_name(theme_name) {
                self.set_theme(theme);
            } else {
                let available = ThemeKind::all()
                    .iter()
                    .map(|theme| theme.name())
                    .collect::<Vec<_>>()
                    .join(", ");
                self.transcript
                    .push(TranscriptItem::Message(ChatMessage::system(format!(
                        "unknown theme: {theme_name}\n\nAvailable themes: {available}"
                    ))));
                self.touch_transcript();
                self.status_line = "unknown theme".to_string();
                self.toast("Unknown theme", ToastKind::Error);
            }
            return true;
        }

        if task == "/tools" {
            self.transcript
                .push(TranscriptItem::Message(ChatMessage::system(tools_text())));
            self.touch_transcript();
            self.status_line = "tool surface listed".to_string();
            self.toast("Tool surface listed", ToastKind::Info);
            return true;
        }

        if task == "/skills" {
            self.transcript
                .push(TranscriptItem::Message(ChatMessage::system(
                    self.tools.skills().list_text(),
                )));
            self.touch_transcript();
            self.status_line = "skills listed".to_string();
            self.toast("Workspace skills listed", ToastKind::Info);
            return true;
        }

        if task == "/agents" {
            self.agent_registry = AgentRegistry::load(self.tools.workspace()).unwrap_or_default();
            self.active_modal = Some(Modal::Agents);
            self.status_line = "named agents".to_string();
            return true;
        }

        if task == "/mcp" || task.starts_with("/mcp ") {
            let args = task.strip_prefix("/mcp").unwrap_or_default().trim();
            if args.is_empty() {
                self.mcp_statuses = self.mcp.statuses();
                self.active_modal = Some(Modal::Mcp);
                self.status_line = "mcp servers".to_string();
                return true;
            }
            let Some(name) = args.strip_prefix("restart ").map(str::trim) else {
                self.status_line = "usage: /mcp [restart <server>]".to_string();
                self.toast("Usage: /mcp [restart <server>]", ToastKind::Warning);
                return true;
            };
            if !self.mcp.has_server(name) {
                self.status_line = "unknown mcp server".to_string();
                self.toast(format!("Unknown MCP server: {name}"), ToastKind::Error);
                return true;
            }
            // Restarting spawns and handshakes (seconds); never on the UI
            // thread. Progress is visible by reopening /mcp.
            let registry = self.mcp.clone();
            let server = name.to_string();
            self.status_line = format!("restarting mcp server {name}");
            self.toast(format!("Restarting MCP server {name}"), ToastKind::Info);
            thread::spawn(move || {
                let _ = registry.restart(&server);
            });
            return true;
        }

        if task == "/auth" {
            self.transcript
                .push(TranscriptItem::Message(ChatMessage::system(
                    self.model.provider_status_lines().join("\n"),
                )));
            self.touch_transcript();
            self.status_line = "auth probed".to_string();
            self.toast("Auth status checked", ToastKind::Success);
            return true;
        }

        if let Some(command) = task.strip_prefix("/exec ") {
            let (command, background) = parse_exec_command(command);
            self.start_exec_command(command, background);
            return true;
        }

        if let Some(path) = task.strip_prefix("/patch ") {
            self.push_tool_start("file.patch".to_string(), path.to_string());
            self.transcript.push(TranscriptItem::Message(ChatMessage::system(format!(
                "permission check · file.patch\nsource: {path}\nrisk: mutation\npreview: reading diff before apply"
            ))));
            self.touch_transcript();
            let result = match self.tools.read_patch_file(path) {
                Ok(diff) => {
                    let preview = diff.lines().take(24).collect::<Vec<_>>().join("\n");
                    self.transcript
                        .push(TranscriptItem::Message(ChatMessage::system(format!(
                            "diff preview · {path}\n{preview}{}",
                            if diff.lines().count() > 24 {
                                "\n…"
                            } else {
                                ""
                            }
                        ))));
                    self.touch_transcript();
                    self.user_tools().file_patch(FilePatchRequest::new(diff))
                }
                Err(error) => Err(error),
            };

            match result {
                Ok(result) => {
                    let files = result.changed_files.join(", ");
                    self.push_tool_result("file.patch", format!("patched files:\n{files}"));
                    self.status_line = "file.patch applied".to_string();
                    self.toast("Patch applied", ToastKind::Success);
                }
                Err(error) => {
                    self.push_tool_result("file.patch", format!("error: {error}"));
                    self.status_line = "file.patch failed".to_string();
                    self.toast("Patch failed", ToastKind::Error);
                }
            }
            return true;
        }

        if task.starts_with('/') {
            let command = task.split_whitespace().next().unwrap_or(task);
            self.transcript
                .push(TranscriptItem::Message(ChatMessage::system(format!(
                    "unknown command: {command}\n\nType /help to see available commands."
                ))));
            self.touch_transcript();
            self.status_line = "unknown command".to_string();
            return true;
        }

        false
    }

    pub(super) fn set_theme(&mut self, theme: ThemeKind) {
        self.theme = theme;
        self.theme_selection = theme_index(theme);
        set_active_theme(theme);
        self.invalidate_render_cache();
        self.status_line = format!("theme: {}", theme.name());
        match save_theme_preference(self.tools.workspace(), theme) {
            Ok(()) => self.toast(
                format!("Theme set to {}", theme.label()),
                ToastKind::Success,
            ),
            Err(error) => self.toast(format!("Theme set, save failed: {error}"), ToastKind::Error),
        }
    }

    pub(super) fn cycle_theme(&mut self, offset: isize) {
        let theme = theme_at_offset(self.theme, offset);
        self.set_theme(theme);
    }

    pub(super) fn set_model_name(&mut self, model: &str) {
        let model = model.trim();
        if model.is_empty() {
            self.toast("Model cannot be empty", ToastKind::Error);
            self.status_line = "model unchanged".to_string();
            return;
        }

        if let Err(error) = self.model.try_set_model_name(model.to_string()) {
            self.toast(format!("Model unavailable: {error}"), ToastKind::Error);
            self.status_line = "model unchanged".to_string();
            return;
        }
        let model = self.model.model_name().to_string();
        self.context_engine
            .set_max_tokens(medusa_core::context::context_max_tokens_for_model(
                self.model.context_window(),
            ));
        let effort = preferred_reasoning_for_model(&model, self.model.reasoning_effort());
        self.model.set_reasoning_effort(effort.clone());
        self.model_selection = model_index(&model);
        self.reasoning_selection = reasoning_index(&model, &effort);
        self.status_line = format!("model: {model}");
        match save_model_picker_preferences(self.tools.workspace(), &model, &effort) {
            Ok(()) => self.toast(format!("Model set to {model}"), ToastKind::Success),
            Err(error) => self.toast(format!("Model set, save failed: {error}"), ToastKind::Error),
        }
    }

    pub(super) fn set_reasoning_effort(&mut self, effort: &str) {
        let effort = effort.trim();
        if effort.is_empty() {
            self.toast("Reasoning effort cannot be empty", ToastKind::Error);
            return;
        }
        self.model.set_reasoning_effort(effort.to_string());
        self.reasoning_selection = reasoning_index(self.model.model_name(), effort);
        self.status_line = format!("reasoning: {effort}");
        match save_reasoning_preference(self.tools.workspace(), effort) {
            Ok(()) => self.toast(
                format!("Reasoning effort set to {effort}"),
                ToastKind::Success,
            ),
            Err(error) => self.toast(
                format!("Effort set, save failed: {error}"),
                ToastKind::Error,
            ),
        }
    }

    pub(super) fn set_permission_mode(&mut self, mode: PermissionMode) {
        let workspace = self.tools.workspace().to_path_buf();
        self.permission_mode = mode;
        self.permission_selection = permission_mode_index(mode);
        self.status_line = format!("permissions: {}", mode.name());

        match save_permission_mode_preference(&workspace, mode).and_then(|_| {
            ToolRuntime::new(&workspace).map(|runtime| (runtime.with_mcp(self.mcp.clone()), ()))
        }) {
            Ok((runtime, ())) => {
                self.tools = runtime;
                self.toast(
                    format!("Permissions set to {}", mode.label()),
                    ToastKind::Success,
                );
            }
            Err(error) => {
                self.toast(
                    format!("Permissions set, reload failed: {error}"),
                    ToastKind::Error,
                );
            }
        }
    }
}
