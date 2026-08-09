use super::*;

impl App {
    pub(super) fn drain_background_job_events(&mut self) -> bool {
        let mut processed = 0usize;
        let mut changed = false;
        while processed < 128 {
            match self.background_job_events.try_recv() {
                Ok(event) => {
                    changed = true;
                    self.apply_background_job_event(event);
                    processed += 1;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }

        changed
    }

    pub(super) fn drain_approval_requests(&mut self) -> bool {
        let mut changed = false;
        let mut processed = 0usize;
        while processed < 128 {
            match self.approval_events.try_recv() {
                Ok(pending) => {
                    changed = true;
                    processed += 1;
                    if self.cancel_requested_at.is_some() {
                        // In-flight approval raced the cancel: deny it so the
                        // parked worker unblocks and sees the token.
                        let _ = pending.respond.send(ApprovalDecision::Deny);
                    } else if let Some(decision) = self.auto_approval_decision(&pending.request) {
                        let _ = pending.respond.send(decision);
                    } else {
                        if self.approval_queue.is_empty() {
                            self.reset_approval_ui();
                        }
                        self.approval_queue.push_back(pending);
                    }
                }
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }

        if changed && !self.approval_queue.is_empty() {
            if self.approval_shown_at.is_none() {
                self.approval_shown_at = Some(Instant::now());
                self.ring_bell_if_due();
            }
            let front = &self.approval_queue[0].request;
            self.status_line = format!("approval required: {}", front.tool.label());
        }
        changed
    }

    /// Session memory: grants approved with "always allow" this session and
    /// exact commands already denied this turn resolve without prompting.
    pub(super) fn auto_approval_decision(
        &self,
        request: &ApprovalRequest,
    ) -> Option<ApprovalDecision> {
        match request.tool {
            ApprovalTool::TerminalExec => {
                let command = request.command.as_deref().unwrap_or("").trim();
                if self.denied_this_turn.iter().any(|denied| denied == command) {
                    return Some(ApprovalDecision::Deny);
                }
                // Sandbox escalations always need a fresh human decision:
                // a stored grant covers running the command, never running
                // it outside the sandbox.
                if request.sandbox_escalation {
                    return None;
                }
                // Match grants against the command with leading env
                // assignments stripped, so a grant on `cargo build` still
                // settles `FOO=bar cargo build`.
                let effective = strip_env_assignments(command);
                if !command_has_shell_tokens(command)
                    && self
                        .session_terminal_grants
                        .iter()
                        .any(|prefix| command_matches_grant(effective, prefix))
                {
                    return Some(ApprovalDecision::AllowOnce);
                }
                None
            }
            ApprovalTool::FileEdit | ApprovalTool::FilePatch => {
                if !request.paths.is_empty()
                    && request
                        .paths
                        .iter()
                        .all(|path| self.denied_edits_this_turn.iter().any(|p| p == path))
                {
                    return Some(ApprovalDecision::Deny);
                }
                let all_granted = !request.paths.is_empty()
                    && request
                        .paths
                        .iter()
                        .all(|path| edit_grant_matches(&self.session_edit_grants, path));
                all_granted.then_some(ApprovalDecision::AllowOnce)
            }
            ApprovalTool::McpTool | ApprovalTool::McpServerLaunch => {
                // Per-tool always-allow grants (and per-session launch
                // approvals) live in the core registry, which skips the gate
                // entirely once granted; here we only auto-deny an identical
                // request re-asked in the same turn after the user said no.
                let command = request.command.as_deref().unwrap_or("").trim();
                if self.denied_this_turn.iter().any(|denied| denied == command) {
                    return Some(ApprovalDecision::Deny);
                }
                None
            }
            ApprovalTool::WebFetch | ApprovalTool::WebSearch => {
                // Once the user always-allows web egress this session, every
                // later fetch/search resolves silently; otherwise re-asked
                // denials are honoured and anything new prompts.
                if self.session_web_egress_allowed {
                    return Some(ApprovalDecision::AllowOnce);
                }
                let command = request.command.as_deref().unwrap_or("").trim();
                if self.denied_this_turn.iter().any(|denied| denied == command) {
                    return Some(ApprovalDecision::Deny);
                }
                None
            }
        }
    }

    pub(super) fn resolve_pending_approval(&mut self, decision: ApprovalDecision) {
        let Some(pending) = self.approval_queue.pop_front() else {
            return;
        };
        // The next queued request must serve its own grace window.
        self.approval_shown_at = None;
        self.approval_selection = 0;
        self.approval_expanded = false;
        self.approval_detail_scroll = 0;

        // Defense in depth: even if an always-allow decision reaches an
        // escalation (the card doesn't offer one), downgrade it to a
        // one-shot approval so nothing is persisted.
        let decision =
            if pending.request.sandbox_escalation && decision == ApprovalDecision::AlwaysAllow {
                ApprovalDecision::AllowOnce
            } else {
                decision
            };

        match decision {
            ApprovalDecision::AlwaysAllow => self.record_always_allow(&pending.request),
            ApprovalDecision::Deny => {
                if let Some(command) = pending.request.command.as_deref() {
                    self.denied_this_turn.push(command.trim().to_string());
                }
                for path in &pending.request.paths {
                    self.denied_edits_this_turn.push(path.clone());
                }
            }
            ApprovalDecision::AllowOnce => {}
        }

        let _ = pending.respond.send(decision);
        self.status_line = match decision {
            ApprovalDecision::AllowOnce => "approved once".to_string(),
            ApprovalDecision::AlwaysAllow => "always allowed".to_string(),
            ApprovalDecision::Deny => "denied".to_string(),
        };

        // A grant can settle other queued requests immediately (bursts from
        // parallel subagents asking for the same thing).
        let mut remaining = std::mem::take(&mut self.approval_queue);
        while let Some(pending) = remaining.pop_front() {
            if let Some(auto) = self.auto_approval_decision(&pending.request) {
                let _ = pending.respond.send(auto);
            } else {
                self.approval_queue.push_back(pending);
            }
        }
        if self.approval_queue.is_empty() {
            self.reset_approval_ui();
        } else {
            // The next queued card is visible immediately, so its accidental
            // keypress guard starts now rather than eating the user's first
            // key whenever they eventually respond.
            self.approval_shown_at = Some(Instant::now());
        }
    }

    pub(super) fn record_always_allow(&mut self, request: &ApprovalRequest) {
        match request.tool {
            ApprovalTool::TerminalExec => {
                let Some(command) = request.command.as_deref() else {
                    return;
                };
                let Some(prefix) = derive_terminal_grant_prefix(command) else {
                    // Complex commands only get allow-once semantics.
                    return;
                };
                self.session_terminal_grants.push(prefix.clone());
                match medusa_core::permissions::PermissionPolicy::append_terminal_allow_prefix(
                    self.tools.workspace(),
                    &prefix,
                ) {
                    Ok(()) => {
                        // Reload so future turns see the persisted grant.
                        if let Ok(reloaded) = ToolRuntime::new(self.tools.workspace()) {
                            self.tools = reloaded.with_mcp(self.mcp.clone());
                        }
                        self.toast(format!("Always allowing `{prefix}`"), ToastKind::Success);
                    }
                    Err(error) => {
                        self.toast(format!("Grant not persisted: {error}"), ToastKind::Warning);
                    }
                }
            }
            ApprovalTool::FileEdit | ApprovalTool::FilePatch => {
                for path in &request.paths {
                    let prefix = path
                        .rsplit_once('/')
                        .map(|(dir, _)| format!("{dir}/"))
                        .unwrap_or_else(|| path.clone());
                    if !self.session_edit_grants.contains(&prefix) {
                        self.session_edit_grants.push(prefix);
                    }
                }
                self.toast(
                    "Always allowing edits there this session",
                    ToastKind::Success,
                );
            }
            ApprovalTool::McpTool => {
                // The core registry recorded the per-(server, tool) grant when
                // authorize returned "always allow"; nothing is persisted to
                // disk for MCP in v1.
                self.toast(
                    "Always allowing that MCP tool this session",
                    ToastKind::Success,
                );
            }
            ApprovalTool::McpServerLaunch => {
                // The core registry marked the server launch-approved for the
                // session; the process is spawned on first use.
                self.toast(
                    "Allowing that MCP server to launch this session",
                    ToastKind::Success,
                );
            }
            ApprovalTool::WebFetch | ApprovalTool::WebSearch => {
                self.session_web_egress_allowed = true;
                self.toast("Allowing web requests this session", ToastKind::Success);
            }
        }
    }

    pub(super) fn apply_background_job_event(&mut self, event: BackgroundJobEvent) {
        match event {
            BackgroundJobEvent::Started {
                id,
                pid,
                command,
                cwd,
            } => {
                self.background_jobs.insert(
                    id.clone(),
                    BackgroundJobView {
                        id: id.clone(),
                        pid,
                        command: command.clone(),
                        cwd,
                        state: ToolRunState::Running,
                        started_at: Instant::now(),
                        finished_at: None,
                        exit_code: None,
                        last_output: String::new(),
                    },
                );
                self.attach_or_push_background_tool_start(&id, &command);
                self.update_tool_result_by_id(
                    &id,
                    ToolRunState::Running,
                    &format!("running · pid {pid}\ncommand: {command}"),
                );
                self.status_line = format!("background shell running · pid {pid}");
                self.toast(self.status_line.clone(), ToastKind::Info);
            }
            BackgroundJobEvent::Finished {
                id,
                pid,
                command,
                cwd,
                code,
                stdout,
                stderr,
            } => {
                let state = if code == Some(0) {
                    ToolRunState::Succeeded
                } else {
                    ToolRunState::Failed
                };
                let detail = compact_tool_detail(&terminal_result_output(&TerminalExecResult {
                    command: command.clone(),
                    cwd,
                    code,
                    stdout,
                    stderr,
                    background: false,
                    pid: Some(pid),
                    job_id: Some(id.clone()),
                    sandboxed: false,
                }));
                if let Some(job) = self.background_jobs.get_mut(&id) {
                    job.state = state;
                    job.finished_at = Some(Instant::now());
                    job.exit_code = code;
                    job.last_output = detail.clone();
                }
                self.update_tool_result_by_id(&id, state, &detail);
                self.status_line = format!(
                    "background shell completed · pid {pid} · exit {}",
                    code.unwrap_or(-1)
                );
                self.toast(self.status_line.clone(), ToastKind::Success);
                self.persist_session();
            }
            BackgroundJobEvent::Failed {
                id,
                pid,
                command,
                error,
                ..
            } => {
                let detail = compact_tool_detail(&format!("command: {command}\nerror: {error}"));
                if let Some(job) = self.background_jobs.get_mut(&id) {
                    job.state = ToolRunState::Failed;
                    job.finished_at = Some(Instant::now());
                    job.last_output = detail.clone();
                }
                self.update_tool_result_by_id(&id, ToolRunState::Failed, &detail);
                self.status_line = format!("background shell failed · pid {pid}");
                self.toast(self.status_line.clone(), ToastKind::Error);
                self.persist_session();
            }
        }
        self.stick_chat_to_bottom_if_needed();
    }

    pub(super) fn apply_workflow_event(&mut self, event: WorkflowEvent) -> bool {
        self.apply_workflow_event_inner(event, true)
    }

    /// Workflow events from a model-launched `workflow_run` tool call: update
    /// the tree but skip the final assistant summary message — the model
    /// receives the result as a tool output and reports it in its own words.
    pub(super) fn apply_workflow_event_from_model(&mut self, event: WorkflowEvent) {
        self.apply_workflow_event_inner(event, false);
    }

    pub(super) fn apply_workflow_event_inner(
        &mut self,
        event: WorkflowEvent,
        announce_summary: bool,
    ) -> bool {
        match event {
            WorkflowEvent::RunStarted {
                run_id,
                title,
                task,
            } => {
                let view = workflow_view_started(run_id, title, task);
                self.set_workflow_status_line(format!("workflow: {}", truncate(&view.title, 48)));
                self.workflows.push(view.clone());
                self.transcript.push(TranscriptItem::Workflow(view));
                self.touch_transcript();
                self.stick_chat_to_bottom_if_needed();
                self.persist_session();
                false
            }
            WorkflowEvent::PhaseStarted {
                run_id,
                phase_index,
                name,
                ..
            } => {
                self.update_workflow(&run_id, |workflow| {
                    workflow.status = WorkflowViewState::Running;
                    // Script workflows create phases dynamically, so unseen
                    // indexes are appended rather than ignored.
                    while workflow.phases.len() <= phase_index {
                        workflow.phases.push(WorkflowPhaseView {
                            name: name.clone(),
                            objective: String::new(),
                            status: WorkflowViewState::Pending,
                            agents: Vec::new(),
                        });
                    }
                    if let Some(phase) = workflow.phases.get_mut(phase_index) {
                        phase.name = name.clone();
                        phase.status = WorkflowViewState::Running;
                    }
                });
                self.set_workflow_status_line(format!("workflow phase: {name}"));
                self.stick_chat_to_bottom_if_needed();
                false
            }
            WorkflowEvent::AgentStarted {
                run_id,
                phase_index,
                agent_index,
                name,
                role,
                tool_policy,
            } => {
                self.update_workflow(&run_id, |workflow| {
                    workflow.status = WorkflowViewState::Running;
                    if let Some(phase) = workflow.phases.get_mut(phase_index) {
                        while phase.agents.len() <= agent_index {
                            phase.agents.push(WorkflowAgentView {
                                name: name.clone(),
                                role: role.clone(),
                                tool_policy,
                                status: WorkflowViewState::Pending,
                                output: String::new(),
                                tool_counts: BTreeMap::new(),
                            });
                        }
                        if let Some(agent) = phase.agents.get_mut(agent_index) {
                            agent.name = name.clone();
                            agent.role = role.clone();
                            agent.tool_policy = tool_policy;
                            agent.status = WorkflowViewState::Running;
                        }
                    }
                });
                self.set_workflow_status_line(format!("subagent: {name}"));
                self.stick_chat_to_bottom_if_needed();
                false
            }
            WorkflowEvent::AgentFinished {
                run_id,
                phase_index,
                agent_index,
                name,
                status,
                output,
                tool_counts,
            } => {
                let state = workflow_state_from_core(status);
                self.update_workflow(&run_id, |workflow| {
                    if let Some(agent) = workflow
                        .phases
                        .get_mut(phase_index)
                        .and_then(|phase| phase.agents.get_mut(agent_index))
                    {
                        agent.status = state;
                        agent.output = compact_tool_detail(&output);
                        agent.tool_counts = tool_counts.clone();
                    }
                });
                self.set_workflow_status_line(format!("subagent complete: {name}"));
                self.stick_chat_to_bottom_if_needed();
                false
            }
            WorkflowEvent::PhaseFinished {
                run_id,
                phase_index,
                name,
                status,
            } => {
                let state = workflow_state_from_core(status);
                self.update_workflow(&run_id, |workflow| {
                    if let Some(phase) = workflow.phases.get_mut(phase_index) {
                        phase.status = state;
                    }
                });
                self.set_workflow_status_line(format!("workflow phase complete: {name}"));
                self.stick_chat_to_bottom_if_needed();
                false
            }
            WorkflowEvent::Log { run_id, message } => {
                let _ = run_id;
                self.set_workflow_status_line(format!("workflow: {message}"));
                false
            }
            WorkflowEvent::RunFinished {
                run_id,
                status,
                summary,
            } => {
                let state = workflow_state_from_core(status);
                self.update_workflow(&run_id, |workflow| {
                    workflow.status = state;
                    workflow.summary = summary.clone();
                });
                if announce_summary {
                    self.transcript
                        .push(TranscriptItem::Message(ChatMessage::assistant(
                            summary.clone(),
                        )));
                    self.touch_transcript();
                }
                let status_line = match status {
                    WorkflowStatus::Succeeded => "workflow complete".to_string(),
                    WorkflowStatus::PartiallySucceeded => "workflow partially complete".to_string(),
                    WorkflowStatus::Running => "workflow running".to_string(),
                    WorkflowStatus::Failed => "workflow failed".to_string(),
                };
                self.set_workflow_status_line(status_line.clone());
                self.toast(
                    status_line,
                    match status {
                        WorkflowStatus::Failed => ToastKind::Error,
                        WorkflowStatus::PartiallySucceeded => ToastKind::Warning,
                        WorkflowStatus::Running | WorkflowStatus::Succeeded => ToastKind::Success,
                    },
                );
                self.stick_chat_to_bottom_if_needed();
                self.persist_session();
                true
            }
        }
    }

    pub(super) fn update_workflow(
        &mut self,
        run_id: &str,
        mut update: impl FnMut(&mut WorkflowRunView),
    ) {
        for workflow in &mut self.workflows {
            if workflow.id == run_id {
                update(workflow);
            }
        }

        let mut transcript_changed = false;
        for item in &mut self.transcript {
            if let TranscriptItem::Workflow(workflow) = item
                && workflow.id == run_id
            {
                update(workflow);
                transcript_changed = true;
            }
        }

        if transcript_changed {
            self.touch_transcript();
        }
        self.persist_session();
    }

    pub(super) fn start_next_queued_turn(&mut self) {
        let Some(task) = self.queued_turns.pop_front() else {
            return;
        };

        if let Some(workflow_task) = task.strip_prefix("/workflow ") {
            self.status_line = "starting queued workflow".to_string();
            self.start_workflow_request(workflow_task);
            return;
        }

        self.transcript
            .push(TranscriptItem::Message(ChatMessage::user(task.clone())));
        self.touch_transcript();
        self.persist_session();
        self.scroll_chat_to_bottom();
        self.status_line = if self.queued_turns.is_empty() {
            "starting queued turn".to_string()
        } else {
            format!("starting queued turn · {} waiting", self.queued_turns.len())
        };
        self.start_model_turn(&task);
    }
}
