use super::*;

impl App {
    pub(super) fn flush_stream_delta(&mut self, delta_buffer: &mut String) {
        if delta_buffer.is_empty() {
            return;
        }

        if let Some(index) = self.streaming_message {
            if let Some(TranscriptItem::Message(message)) = self.transcript.get_mut(index) {
                message.content.push_str(delta_buffer);
                self.touch_transcript();
            }
        } else {
            let index = self.transcript.len();
            self.transcript
                .push(TranscriptItem::Message(ChatMessage::assistant(
                    std::mem::take(delta_buffer),
                )));
            self.touch_transcript();
            self.streaming_message = Some(index);
            return;
        }

        delta_buffer.clear();
        self.stick_chat_to_bottom_if_needed();
        delta_buffer.clear();
    }

    pub(super) fn push_tool_start(&mut self, name: String, summary: String) {
        self.push_tool_start_with_id(None, name, summary);
    }

    pub(super) fn push_tool_start_with_id(
        &mut self,
        id: Option<String>,
        name: String,
        summary: String,
    ) {
        let run = ToolRun {
            id,
            started_at: Instant::now(),
            pending_result: None,
            name,
            summary,
            state: ToolRunState::Running,
            detail: String::new(),
            expanded: false,
            group_expanded: false,
        };
        self.transcript.push(TranscriptItem::Tool(run));
        self.touch_transcript();
        self.persist_session();
    }

    pub(super) fn push_tool_result(&mut self, name: &str, output: String) {
        let state = if tool_output_failed(&output) {
            ToolRunState::Failed
        } else {
            ToolRunState::Succeeded
        };
        let detail = compact_tool_detail(&output);
        self.update_transcript_tool_result(name, state, &detail);
        self.persist_session();
    }

    /// Resolve a tool result to its transcript block by call id — required for
    /// parallel calls, where two same-named runs can be in flight at once and
    /// "most recent running with this name" would misattribute results.
    pub(super) fn push_tool_result_for_call(&mut self, call_id: &str, name: &str, output: String) {
        let state = if tool_output_failed(&output) {
            ToolRunState::Failed
        } else {
            ToolRunState::Succeeded
        };
        let detail = compact_tool_detail(&output);

        if let Some(run) = self
            .transcript
            .iter_mut()
            .rev()
            .find_map(|item| match item {
                TranscriptItem::Tool(run)
                    if run.id.as_deref() == Some(call_id) && run.state == ToolRunState::Running =>
                {
                    Some(run)
                }
                _ => None,
            })
        {
            queue_or_apply_tool_result(run, state, detail, state == ToolRunState::Failed);
            self.touch_transcript();
            self.persist_session();
            return;
        }

        // No started block carries this id (e.g. restored session) — fall
        // back to the name-based path, which also creates a block if needed.
        self.update_transcript_tool_result(name, state, &detail);
        self.persist_session();
    }

    pub(super) fn update_transcript_tool_result(
        &mut self,
        name: &str,
        state: ToolRunState,
        detail: &str,
    ) {
        if let Some(run) = self
            .transcript
            .iter_mut()
            .rev()
            .find_map(|item| match item {
                TranscriptItem::Tool(run)
                    if run.name == name && run.state == ToolRunState::Running =>
                {
                    Some(run)
                }
                _ => None,
            })
        {
            queue_or_apply_tool_result(run, state, detail.to_string(), false);
            self.touch_transcript();
            return;
        }

        self.transcript.push(TranscriptItem::Tool(ToolRun {
            id: None,
            started_at: Instant::now(),
            pending_result: None,
            name: name.to_string(),
            summary: String::new(),
            state,
            detail: detail.to_string(),
            expanded: false,
            group_expanded: false,
        }));
        self.touch_transcript();
    }

    pub(super) fn conversation_history(&self) -> Vec<ConversationMessage> {
        let mut messages = Vec::new();
        messages.push(ConversationMessage {
            role: "system".to_string(),
            content: permission_context_text(self.permission_mode).to_string(),
            attachments: Vec::new(),
        });
        if self.plan_mode {
            messages.push(ConversationMessage {
                role: "system".to_string(),
                content: PLAN_MODE_DIRECTIVE.to_string(),
                attachments: Vec::new(),
            });
        }

        // Keep this history stable for ContextEngine index accounting. The
        // regenerated rolling session state is inserted after compaction in
        // start_model_turn, immediately before the latest user message.
        messages.extend(self.recent_conversation_messages());
        messages
    }

    /// Full conversation history; token budgeting and compaction happen in
    /// the ContextEngine at turn start, not by windowing here.
    pub(super) fn recent_conversation_messages(&self) -> Vec<ConversationMessage> {
        self.transcript
            .iter()
            .filter_map(transcript_conversation_message)
            .collect()
    }

    pub(super) fn session_state_context_text(&self) -> String {
        session_state_context_text(
            &self.transcript,
            self.recent_conversation_messages().len(),
            SessionStateRuntime {
                workspace: &self.cwd_display,
                model: self.model.model_name(),
                permission_mode: self.permission_mode,
                status: &self.status_line,
                workflows: &self.workflows,
                active_workflows: self.workflow_events.len(),
                background_jobs: &self.background_jobs,
            },
        )
    }

    pub(super) fn persist_session(&mut self) {
        if let Some(session) = &self.session
            && let Err(error) = session.save_transcript(&self.transcript)
        {
            self.status_line = format!("session save failed: {error}");
        }
    }

    pub(super) fn toast(&mut self, message: impl Into<String>, kind: ToastKind) {
        self.toast = Some(Toast {
            message: message.into(),
            kind,
            created_at: Instant::now(),
        });
    }

    pub(super) fn animation_frame(&self) -> u64 {
        // Keep the working indicator feeling active, not stuck.
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| (duration.as_millis() / 110) as u64)
    }

    pub(super) fn expire_toast(&mut self) -> bool {
        if self
            .toast
            .as_ref()
            .is_some_and(|toast| toast.created_at.elapsed() > Duration::from_secs(3))
        {
            self.toast = None;
            return true;
        }

        false
    }
}
