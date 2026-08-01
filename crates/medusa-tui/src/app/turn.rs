use super::*;

impl App {
    pub(super) fn start_model_turn(&mut self, _task: &str) {
        if !self.model_enabled {
            self.status_line = "queued".to_string();
            return;
        }

        if self.model_events.is_some() || self.streaming_message.is_some() {
            self.queued_turns.push_back(_task.to_string());
            self.status_line = format!(
                "queued: {}{}",
                truncate(_task, 48),
                queue_count_suffix(self.queued_turns.len())
            );
            return;
        }

        let assistant_index = self.transcript.len();
        self.transcript
            .push(TranscriptItem::Message(ChatMessage::assistant("")));
        self.touch_transcript();
        self.persist_session();
        self.streaming_message = Some(assistant_index);
        self.last_stream_save = Instant::now();
        self.status_line = self.scoped_status("streaming");
        self.turn_started_at = Some(Instant::now());

        self.denied_this_turn.clear();
        self.denied_edits_this_turn.clear();
        self.turn_usage = TokenUsage::default();
        self.turn_requests = 0;
        let backend = self.model.clone();
        let permission_mode = self.permission_mode;
        // Per-turn checkpoint recorder: mutating file tools capture pre-images
        // through it, keyed to this turn's user message row.
        let transcript_user_index = self.transcript[..assistant_index]
            .iter()
            .rposition(|item| {
                matches!(
                    item,
                    TranscriptItem::Message(ChatMessage {
                        role: ChatRole::User,
                        ..
                    })
                )
            })
            .unwrap_or(0);
        let recorder = CheckpointRecorder::new(
            self.tools.workspace(),
            CheckpointMeta {
                session_id: self
                    .session
                    .as_ref()
                    .map(SessionStore::current_id)
                    .unwrap_or_default(),
                prompt_excerpt: excerpt_for_checkpoint(_task),
                transcript_user_index,
            },
        );
        self.active_checkpoint = Some(recorder.clone());
        let cancel = CancelToken::new();
        self.turn_cancel = Some(cancel.clone());
        self.cancel_requested_at = None;
        let tools = self
            .tools
            .clone()
            .with_background_events(self.background_job_sender.clone())
            .with_approval_handler(self.approval_handler.clone())
            .with_checkpoint_recorder(recorder)
            .with_cancel_token(cancel.clone());
        #[cfg(test)]
        {
            self.last_turn_runtime = Some(tools.clone());
        }
        let history = self.conversation_history();
        let session_state = self.session_state_context_text();
        let context_engine = self.context_engine.clone();
        let plan_mode = self.plan_mode;
        let (sender, receiver) = mpsc::channel();
        self.model_events = Some(receiver);

        thread::spawn(move || {
            // Compaction may call the model to summarize old history, so it
            // runs here on the worker thread, never on the UI thread. It
            // shares the turn's cancel token so Esc interrupts it too.
            let mut prompt = context_engine.prepare(&history, &backend, &cancel);
            insert_runtime_session_state(&mut prompt, session_state);
            let result = if permission_mode == PermissionMode::Readonly || plan_mode {
                backend.chat_stream_messages_read_only(&prompt, tools, |event| {
                    sender.send(event).map_err(|error| {
                        color_eyre::eyre::eyre!("failed to send stream event: {error}")
                    })?;
                    Ok(())
                })
            } else {
                backend.chat_stream_messages(&prompt, tools, |event| {
                    sender.send(event).map_err(|error| {
                        color_eyre::eyre::eyre!("failed to send stream event: {error}")
                    })?;
                    Ok(())
                })
            };

            match result {
                Ok(event_count) => {
                    let _ = sender.send(ModelStreamEvent::Done { event_count });
                }
                Err(error) if error_is_cancellation(&error) => {
                    let _ = sender.send(ModelStreamEvent::Cancelled);
                }
                Err(error) => {
                    let _ = sender.send(ModelStreamEvent::Error(error.to_string()));
                }
            }
        });
    }

    pub(super) fn is_working(&self) -> bool {
        self.model_events.is_some() || self.streaming_message.is_some()
    }

    /// BEL when a long-running turn needs attention (approval prompt) or
    /// ends (complete/error/cancel); [`should_ring_bell`] holds the gating.
    pub(super) fn ring_bell_if_due(&self) {
        let enabled = bell_enabled(self.bell_setting, env::var("MEDUSA_BELL").ok().as_deref());
        let working_for = self.turn_started_at.map(|started| started.elapsed());
        if should_ring_bell(enabled, working_for) {
            let mut stdout = io::stdout();
            let _ = stdout.write_all(b"\x07");
            let _ = stdout.flush();
        }
    }

    /// First Esc while working: flip the turn's cancel token and unblock a
    /// worker that may be parked on an approval prompt by denying everything
    /// queued. The worker unwinds cooperatively and reports Cancelled.
    pub(super) fn request_cancel_turn(&mut self) {
        if let Some(token) = &self.turn_cancel {
            token.cancel();
        }
        while let Some(pending) = self.approval_queue.pop_front() {
            let _ = pending.respond.send(ApprovalDecision::Deny);
        }
        self.approval_shown_at = None;
        self.cancel_requested_at = Some(Instant::now());
        self.status_line = "cancelling… esc again to force-stop".to_string();
    }

    /// Second Esc while cancelling: stop waiting for the worker. Dropping the
    /// receiver makes its next send fail, so the thread dies on its own.
    pub(super) fn force_abandon_turn(&mut self) {
        self.model_events = None;
        self.finalize_cancelled_turn("turn abandoned");
    }

    /// Close out an interrupted turn: resolve every still-running transcript
    /// row, leave a muted system note (which also re-enters model history so
    /// the conversation resumes coherently), and clear all turn state.
    /// Partial assistant text stays in the transcript.
    /// Freeze the streaming turn's usage into the last-turn readout. Guarded
    /// so a turn that failed before any request keeps the previous readout.
    pub(super) fn record_turn_usage_totals(&mut self) {
        if self.turn_requests > 0 {
            self.last_turn_usage = self.turn_usage;
            self.last_turn_requests = self.turn_requests;
        }
    }

    pub(super) fn finalize_cancelled_turn(&mut self, status: &str) {
        self.record_turn_usage_totals();
        self.ring_bell_if_due();
        for item in &mut self.transcript {
            match item {
                TranscriptItem::Tool(run) if run.state == ToolRunState::Running => {
                    apply_tool_result_now(
                        run,
                        ToolRunState::Failed,
                        "cancelled".to_string(),
                        false,
                    );
                }
                TranscriptItem::Workflow(view) => mark_workflow_view_cancelled(view),
                _ => {}
            }
        }
        // The turn's views are marked cancelled above; actually stop the
        // background workflow workers so their file-mutating tools bail (their
        // checkpoints are then finalized when the receiver disconnects).
        for workflow in &self.workflow_events {
            workflow.cancel.cancel();
        }
        self.transcript
            .push(TranscriptItem::Message(ChatMessage::system(
                TURN_INTERRUPTED_NOTE,
            )));
        self.touch_transcript();
        self.stick_chat_to_bottom_if_needed();

        self.streaming_message = None;
        self.turn_started_at = None;
        self.turn_cancel = None;
        self.cancel_requested_at = None;
        self.status_line = status.to_string();
        // Keep the follow-up prompts the user explicitly queued (the UI
        // acknowledged each with "queued: …"). Silently dropping them on cancel
        // loses text the user believes is pending; instead we hold them and say
        // so — an empty submit while idle runs the next one.
        if !self.queued_turns.is_empty() {
            let count = self.queued_turns.len();
            self.toast(
                format!(
                    "{count} queued prompt{} kept — submit an empty line to run",
                    if count == 1 { "" } else { "s" }
                ),
                ToastKind::Info,
            );
        }
        self.persist_session();
        self.finish_turn_checkpoint();
    }

    pub(super) fn has_active_workflows(&self) -> bool {
        !self.workflow_events.is_empty()
    }

    /// Cancel every background workflow: flip each run's shared cancel token so
    /// its subagents' model streams and file-mutating tools bail cooperatively,
    /// and mark the visible workflow rows cancelled now. The workers are
    /// removed from `workflow_events` once their receivers disconnect
    /// (`drain_workflow_events`), which also finalizes their checkpoints; the JS
    /// orchestration loop may run to its next await before it observes the
    /// token, so the rows show "cancelled" a beat before the thread exits.
    pub(super) fn cancel_active_workflows(&mut self) {
        if self.workflow_events.is_empty() {
            return;
        }
        let count = self.workflow_events.len();
        for workflow in &self.workflow_events {
            workflow.cancel.cancel();
        }
        for view in &mut self.workflows {
            mark_workflow_view_cancelled(view);
        }
        let mut transcript_changed = false;
        for item in &mut self.transcript {
            if let TranscriptItem::Workflow(view) = item {
                mark_workflow_view_cancelled(view);
                transcript_changed = true;
            }
        }
        if transcript_changed {
            self.touch_transcript();
        }
        self.status_line = "cancelling background workflow…".to_string();
        self.toast(
            format!(
                "Cancelling {count} background workflow{}",
                if count == 1 { "" } else { "s" }
            ),
            ToastKind::Warning,
        );
    }

    /// `/compact`: fold older history into the ContextEngine summary now
    /// instead of waiting for the budget to force it. Summarization calls the
    /// model, so it runs on a worker thread; the result lands via
    /// [`Self::drain_compact_events`].
    pub(super) fn run_compact_command(&mut self) {
        if self.is_working() {
            self.status_line = "compact unavailable while a turn is running".to_string();
            self.toast("Cannot compact while a turn is running", ToastKind::Warning);
            return;
        }
        if self.compact_events.is_some() {
            self.status_line = "compaction already running".to_string();
            self.toast("Compaction already running", ToastKind::Info);
            return;
        }
        if !self.model_enabled {
            self.status_line = "compact needs the model backend".to_string();
            self.toast("Compaction needs the model backend", ToastKind::Warning);
            return;
        }

        let history = self.conversation_history();
        let engine = self.context_engine.clone();
        let backend = self.model.clone();
        let (sender, receiver) = mpsc::channel();
        self.compact_events = Some(receiver);
        self.status_line = "compacting context…".to_string();

        thread::spawn(move || {
            let cancel = CancelToken::new();
            let result = engine
                .compact_now(&history, &backend, &cancel)
                .map_err(|error| error.to_string());
            let _ = sender.send(result);
        });
    }

    pub(super) fn drain_compact_events(&mut self) -> bool {
        let Some(receiver) = &self.compact_events else {
            return false;
        };
        let outcome = match receiver.try_recv() {
            Ok(outcome) => outcome,
            Err(TryRecvError::Empty) => return false,
            Err(TryRecvError::Disconnected) => Err("compaction worker exited".to_string()),
        };
        self.compact_events = None;
        match outcome {
            Ok(compaction) => {
                self.status_line = "context compacted".to_string();
                self.toast(
                    format!(
                        "Compacted: est. {} → {} ({} messages folded)",
                        format_token_count(compaction.before_tokens as u64),
                        format_token_count(compaction.after_tokens as u64),
                        compaction.folded_messages
                    ),
                    ToastKind::Success,
                );
            }
            Err(error) => {
                self.status_line = "compact failed".to_string();
                self.toast(format!("Compact failed: {error}"), ToastKind::Error);
            }
        }
        true
    }

    pub(super) fn drain_pending_tool_results(&mut self) -> bool {
        let mut changed = false;
        for item in &mut self.transcript {
            if let TranscriptItem::Tool(run) = item
                && run.state == ToolRunState::Running
                && run.started_at.elapsed() >= MIN_TOOL_PULSE_VISIBLE
                && let Some(pending) = run.pending_result.take()
            {
                let expand = pending.state == ToolRunState::Failed;
                apply_tool_result_now(run, pending.state, pending.detail, expand);
                changed = true;
            }
        }

        if changed {
            self.touch_transcript();
            self.persist_session();
        }

        changed
    }

    pub(super) fn has_running_tool_rows(&self) -> bool {
        self.transcript.iter().any(|item| {
            matches!(
                item,
                TranscriptItem::Tool(ToolRun {
                    state: ToolRunState::Running,
                    ..
                })
            )
        })
    }

    pub(super) fn has_running_workflow_rows(&self) -> bool {
        self.transcript.iter().any(|item| {
            matches!(
                item,
                TranscriptItem::Workflow(WorkflowRunView {
                    status: WorkflowViewState::Running,
                    ..
                })
            )
        })
    }

    pub(super) fn has_active_animation(&self) -> bool {
        self.is_working()
            || self.has_active_workflows()
            || self.has_running_tool_rows()
            || self.has_running_workflow_rows()
    }

    pub(super) fn touch_transcript(&mut self) {
        self.transcript_version = self.transcript_version.wrapping_add(1);
        self.transcript_rows_cache = None;
        self.last_transcript_rows = Arc::new(Vec::new());
    }

    pub(super) fn invalidate_render_cache(&mut self) {
        self.transcript_rows_cache = None;
        self.last_transcript_rows = Arc::new(Vec::new());
        self.attachment_previews.clear();
    }

    pub(super) fn set_workflow_status_line(&mut self, status: impl Into<String>) {
        if !self.is_working() {
            self.status_line = status.into();
        }
    }

    pub(super) fn request_reload(&mut self) {
        if self.is_working() || self.has_active_workflows() {
            self.status_line = "reload blocked: work is still running".to_string();
            self.toast("Wait for active work before reloading", ToastKind::Warning);
            return;
        }

        if !self.queued_turns.is_empty() {
            self.status_line = "reload blocked: queued turns would be lost".to_string();
            self.toast("Finish queued turns before reloading", ToastKind::Warning);
            return;
        }

        self.persist_session();
        if let Err(error) = save_theme_preference(self.tools.workspace(), self.theme) {
            self.toast(
                format!("Theme save failed before reload: {error}"),
                ToastKind::Warning,
            );
        }
        // Also pass the active theme through the process environment. This covers reloads
        // from older builds that did not persist theme settings yet, or workspaces where
        // writing .medusa/settings.json failed.
        unsafe { env::set_var("MEDUSA_RELOAD_THEME", self.theme.name()) };
        self.status_line = "reloading Medusa…".to_string();
        self.toast("Reloading Medusa", ToastKind::Info);
        self.restart_requested = true;
        self.should_quit = true;
    }

    pub(super) fn start_workflow_request(&mut self, request: &str) {
        let request = request.trim();
        let (first, rest) = match request.split_once(char::is_whitespace) {
            Some((first, rest)) => (first, rest.trim()),
            None => (request, ""),
        };
        if WorkflowScript::list(self.tools.workspace()).contains(&first.to_string()) {
            self.start_workflow_script(first, rest);
        } else {
            self.start_model_authored_workflow(request);
        }
    }

    pub(super) fn start_model_authored_workflow(&mut self, task: &str) {
        if task.trim().is_empty() {
            self.status_line = "workflow needs a task".to_string();
            self.toast("Workflow task required", ToastKind::Warning);
            return;
        }

        if !self.model_enabled {
            self.status_line = "workflow queued".to_string();
            return;
        }

        if self.is_working() {
            self.queued_turns
                .push_back(format!("/workflow {}", task.trim()));
            self.status_line = format!(
                "queued workflow: {}{}",
                truncate(task.trim(), 44),
                queue_count_suffix(self.queued_turns.len())
            );
            return;
        }

        let command = format!("/workflow {}", task.trim());
        self.transcript
            .push(TranscriptItem::Message(ChatMessage::user(command.clone())));
        self.touch_transcript();
        self.persist_session();
        self.scroll_chat_to_bottom();

        self.status_line = "authoring dynamic workflow".to_string();
        self.start_model_turn(&command);
    }

    pub(super) fn start_workflow_script(&mut self, name: &str, raw_args: &str) {
        if !self.model_enabled {
            self.status_line = "workflow queued".to_string();
            return;
        }

        if self.is_working() {
            let queued = if raw_args.is_empty() {
                format!("/workflow {name}")
            } else {
                format!("/workflow {name} {raw_args}")
            };
            self.queued_turns.push_back(queued);
            self.status_line = format!(
                "queued workflow script: {name}{}",
                queue_count_suffix(self.queued_turns.len())
            );
            return;
        }

        let script = match WorkflowScript::load(self.tools.workspace(), name) {
            Ok(script) => script,
            Err(error) => {
                self.status_line = "workflow script failed to load".to_string();
                self.toast(format!("Workflow script error: {error}"), ToastKind::Error);
                return;
            }
        };

        let args = if raw_args.is_empty() {
            None
        } else {
            Some(
                serde_json::from_str(raw_args)
                    .unwrap_or_else(|_| serde_json::Value::String(raw_args.to_string())),
            )
        };

        let command = format!(
            "/workflow {name}{}{raw_args}",
            if raw_args.is_empty() { "" } else { " " }
        );
        let user_index = self.transcript.len();
        self.transcript
            .push(TranscriptItem::Message(ChatMessage::user(command.clone())));
        self.touch_transcript();
        self.persist_session();
        self.scroll_chat_to_bottom();

        let runtime = WorkflowRuntime::new(self.tools.workspace().to_path_buf())
            .with_memory_context(self.session_state_context_text());
        self.denied_this_turn.clear();
        self.denied_edits_this_turn.clear();
        let backend = self.model.clone();
        // Per-run checkpoint recorder + cancel token: see `start_workflow`.
        let recorder = self.new_workflow_checkpoint(&command, user_index);
        let cancel = CancelToken::new();
        let tools = self
            .tools
            .clone()
            .with_approval_handler(self.approval_handler.clone())
            .with_checkpoint_recorder(recorder.clone())
            .with_cancel_token(cancel.clone());
        #[cfg(test)]
        {
            self.last_workflow_runtime = Some(tools.clone());
        }
        let (sender, receiver) = mpsc::channel();
        self.workflow_events.push(BackgroundWorkflow {
            events: receiver,
            checkpoint: recorder,
            cancel,
        });
        self.status_line = format!("workflow script starting: {name}");
        self.toast("Workflow script started", ToastKind::Info);

        thread::spawn(move || {
            let result = runtime.run_script(&script, args, backend, tools, |event| {
                sender.send(event).map_err(|error| {
                    color_eyre::eyre::eyre!("failed to send workflow event: {error}")
                })?;
                Ok(())
            });

            if let Err(error) = result {
                let _ = sender.send(WorkflowEvent::RunFinished {
                    run_id: "workflow-error".to_string(),
                    status: WorkflowStatus::Failed,
                    summary: format!("workflow script failed: {error}"),
                });
            }
        });
    }

    pub(super) fn drain_model_events(&mut self) -> bool {
        let Some(receiver) = self.model_events.take() else {
            return false;
        };

        let mut keep_receiver = true;
        let mut processed = 0usize;
        let mut turn_finished = false;
        let mut delta_buffer = String::new();
        let mut changed = false;

        while processed < 256 {
            match receiver.try_recv() {
                Ok(ModelStreamEvent::Delta(delta)) => {
                    changed = true;
                    delta_buffer.push_str(&delta);
                    self.status_line = self.scoped_status("streaming");
                }
                Ok(ModelStreamEvent::ReasoningDelta(delta)) => {
                    changed = true;
                    let _ = delta;
                    self.flush_stream_delta(&mut delta_buffer);
                    self.stick_chat_to_bottom_if_needed();
                }
                Ok(ModelStreamEvent::ToolStart {
                    call_id,
                    name,
                    summary,
                }) => {
                    changed = true;
                    self.flush_stream_delta(&mut delta_buffer);
                    self.stick_chat_to_bottom_if_needed();
                    self.streaming_message = None;
                    if name == "task.update" {
                        self.status_line = self.scoped_status(summary);
                    } else if name == "plan.update" {
                        self.status_line = self.scoped_status("updating plan");
                    } else if name == "decision.request" {
                        self.status_line = self.scoped_status("waiting on planning decision");
                    } else {
                        self.push_tool_start_with_id(Some(call_id), name.clone(), summary);
                        self.status_line = self.scoped_status(format!("running {name}"));
                    }
                    self.stick_chat_to_bottom_if_needed();
                }
                Ok(ModelStreamEvent::ToolResult {
                    call_id,
                    name,
                    output,
                }) => {
                    changed = true;
                    self.flush_stream_delta(&mut delta_buffer);
                    if name == "task.update" {
                        self.status_line = self.scoped_status(output);
                    } else if name == "plan.update" {
                        match self.apply_plan_update_output(&output) {
                            Ok(()) => {
                                self.status_line = self.scoped_status("plan updated");
                            }
                            Err(error) => {
                                self.push_tool_result(&name, format!("error: {error}"));
                                self.status_line = self.scoped_status("plan.update failed");
                            }
                        }
                    } else if name == "decision.request" {
                        match self.apply_decision_request_output(&output) {
                            Ok(()) => {
                                self.status_line = self.scoped_status("decision requested");
                            }
                            Err(error) => {
                                self.push_tool_result(&name, format!("error: {error}"));
                                self.status_line = self.scoped_status("decision.request failed");
                            }
                        }
                    } else {
                        // Parallel calls complete out of order; call_id pins
                        // the result to the right transcript block.
                        self.push_tool_result_for_call(&call_id, &name, output);
                        self.status_line = self.scoped_status(format!("{name} complete"));
                    }
                    self.stick_chat_to_bottom_if_needed();
                }
                Ok(ModelStreamEvent::Workflow(event)) => {
                    changed = true;
                    self.flush_stream_delta(&mut delta_buffer);
                    self.apply_workflow_event_from_model(event);
                    self.stick_chat_to_bottom_if_needed();
                }
                Ok(ModelStreamEvent::Usage(usage)) => {
                    // One event per model request; a tool-looping turn sends
                    // several, so sum them for turn and session totals.
                    changed = true;
                    self.turn_usage.add(usage);
                    self.turn_requests += 1;
                    self.session_usage.add(usage);
                    self.session_requests += 1;
                }
                Ok(ModelStreamEvent::Done { event_count }) => {
                    changed = true;
                    self.flush_stream_delta(&mut delta_buffer);
                    if self.cancel_requested_at.is_some() {
                        // Esc raced the natural finish and the user asked to
                        // stop: honor the stop intent. Finalize as an
                        // interruption (which keeps queued prompts per [21])
                        // and — crucially — do NOT set `turn_finished`, so the
                        // tail never auto-launches the next queued turn. A
                        // cancel intent must never silently start more work.
                        self.finalize_cancelled_turn("turn interrupted");
                        keep_receiver = false;
                        break;
                    }
                    self.record_turn_usage_totals();
                    self.status_line =
                        self.scoped_status(format!("complete ({event_count} events)"));
                    self.stick_chat_to_bottom_if_needed();
                    self.streaming_message = None;
                    self.ring_bell_if_due();
                    self.turn_started_at.take();
                    self.turn_cancel = None;
                    keep_receiver = false;
                    turn_finished = true;
                    break;
                }
                Ok(ModelStreamEvent::Cancelled) => {
                    changed = true;
                    self.flush_stream_delta(&mut delta_buffer);
                    self.finalize_cancelled_turn("turn interrupted");
                    keep_receiver = false;
                    break;
                }
                // Post-cancel socket/send errors are fallout from the user's
                // own Esc — render them as the interruption they are, never
                // as a scary failure toast.
                Ok(ModelStreamEvent::Error(_)) if self.cancel_requested_at.is_some() => {
                    changed = true;
                    self.flush_stream_delta(&mut delta_buffer);
                    self.finalize_cancelled_turn("turn interrupted");
                    keep_receiver = false;
                    break;
                }
                Ok(ModelStreamEvent::Error(error)) => {
                    changed = true;
                    self.flush_stream_delta(&mut delta_buffer);
                    self.record_turn_usage_totals();
                    self.ring_bell_if_due();
                    let clean_error = clean_model_error(&error);
                    if let Some(index) = self.streaming_message {
                        if let Some(TranscriptItem::Message(message)) =
                            self.transcript.get_mut(index)
                        {
                            if !message.content.is_empty() {
                                message.content.push('\n');
                            }
                            message.content.push_str(&clean_error);
                            self.touch_transcript();
                        }
                    } else {
                        self.transcript
                            .push(TranscriptItem::Message(ChatMessage::system(
                                clean_error.clone(),
                            )));
                        self.touch_transcript();
                    }
                    self.persist_session();
                    self.status_line = model_error_status(&clean_error).to_string();
                    self.toast(clean_error, ToastKind::Error);
                    self.streaming_message = None;
                    self.turn_cancel = None;
                    keep_receiver = false;
                    turn_finished = true;
                    break;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    changed = true;
                    self.flush_stream_delta(&mut delta_buffer);
                    if self.cancel_requested_at.is_some() {
                        // Worker died mid-cancel without a final event.
                        self.finalize_cancelled_turn("turn interrupted");
                        keep_receiver = false;
                        break;
                    }
                    if self.streaming_message.is_some() {
                        self.status_line = self.scoped_status("stream ended");
                    }
                    self.record_turn_usage_totals();
                    self.ring_bell_if_due();
                    self.streaming_message = None;
                    self.turn_cancel = None;
                    keep_receiver = false;
                    turn_finished = true;
                    break;
                }
            }

            processed += 1;
        }

        self.flush_stream_delta(&mut delta_buffer);
        self.stick_chat_to_bottom_if_needed();

        if self.streaming_message.is_some()
            && self.last_stream_save.elapsed() >= Duration::from_millis(750)
        {
            self.persist_session();
            self.last_stream_save = Instant::now();
        }

        if keep_receiver {
            self.model_events = Some(receiver);
        } else if turn_finished {
            self.finish_turn_checkpoint();
            self.start_next_queued_turn();
        }

        changed
    }

    /// Build a per-run checkpoint recorder for a background workflow, keyed to
    /// the workflow command's user-message row. Mirrors the model-turn recorder
    /// so `/rewind` treats workflow edits exactly like model-turn edits.
    pub(super) fn new_workflow_checkpoint(
        &self,
        command: &str,
        user_index: usize,
    ) -> CheckpointRecorder {
        CheckpointRecorder::new(
            self.tools.workspace(),
            CheckpointMeta {
                session_id: self
                    .session
                    .as_ref()
                    .map(SessionStore::current_id)
                    .unwrap_or_default(),
                prompt_excerpt: excerpt_for_checkpoint(command),
                transcript_user_index: user_index,
            },
        )
    }

    /// Close out a checkpoint recorder (model turn or workflow): a quiet
    /// status-line note when files were captured (never a transcript row),
    /// then retention pruning. Returns the summary when anything was captured.
    pub(super) fn finalize_checkpoint(
        &mut self,
        recorder: &CheckpointRecorder,
    ) -> Option<CheckpointSummary> {
        let summary = recorder.finish()?;
        self.status_line = format!(
            "checkpoint · {} file{}",
            summary.file_count,
            if summary.file_count == 1 { "" } else { "s" }
        );
        if let Ok(store) = CheckpointStore::open(self.tools.workspace())
            && let Err(error) = store.prune(RetentionLimits::from_env())
        {
            self.status_line = format!("checkpoint prune failed: {error}");
        }
        Some(summary)
    }

    pub(super) fn finish_turn_checkpoint(&mut self) {
        let Some(recorder) = self.active_checkpoint.take() else {
            return;
        };
        self.finalize_checkpoint(&recorder);
    }

    pub(super) fn drain_workflow_events(&mut self) -> bool {
        if self.workflow_events.is_empty() {
            return false;
        };

        let workflows = std::mem::take(&mut self.workflow_events);
        let mut active_workflows = Vec::new();
        let mut any_finished = false;
        let mut changed = false;

        for workflow in workflows {
            let mut keep_receiver = true;
            let mut processed = 0usize;
            let mut finished = false;

            while processed < 256 {
                match workflow.events.try_recv() {
                    Ok(event) => {
                        changed = true;
                        finished |= self.apply_workflow_event(event);
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        changed = true;
                        keep_receiver = false;
                        finished = true;
                        if !self.is_working()
                            && (self.status_line == "background workflow starting"
                                || self.status_line.contains("workflow"))
                        {
                            self.status_line = "background workflow ended".to_string();
                        }
                        break;
                    }
                }

                processed += 1;
            }

            if keep_receiver && !finished {
                active_workflows.push(workflow);
            } else if finished {
                any_finished = true;
                // The worker thread is gone: close out its checkpoint so the
                // captured pre-images are pruned like a model turn's. The
                // manifest was already written on each capture, so /rewind can
                // undo the run even if this prune never ran.
                self.finalize_checkpoint(&workflow.checkpoint);
            }
        }

        self.workflow_events = active_workflows;
        if any_finished {
            self.persist_session();
            // A background workflow finishing must also drain the queue —
            // otherwise turns queued while it ran strand forever (the only
            // other dequeue site is a model turn's completion). Start the next
            // queued turn only when the app is fully idle: no streaming model
            // turn and no remaining background workflow.
            if !self.is_working() && !self.has_active_workflows() {
                self.start_next_queued_turn();
            }
        }

        changed
    }
}
