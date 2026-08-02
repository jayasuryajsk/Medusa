use super::*;

impl App {
    pub(super) fn permission_status_prefix(&self) -> Option<&'static str> {
        match self.permission_mode {
            PermissionMode::Open => None,
            PermissionMode::Guarded => Some("guarded"),
            PermissionMode::Ask => Some("ask"),
            PermissionMode::Readonly => Some("readonly"),
        }
    }

    pub(super) fn scoped_status(&self, status: impl AsRef<str>) -> String {
        match self.permission_status_prefix() {
            Some(prefix) => format!("{prefix} · {}", status.as_ref()),
            None => status.as_ref().to_string(),
        }
    }

    pub(super) fn resume_session(&mut self, session_id: &str) {
        if self.model_events.is_some() {
            self.status_line = "finish current turn before resuming".to_string();
            self.toast("Cannot resume while working", ToastKind::Warning);
            return;
        }

        if session_id.is_empty() {
            self.status_line = "resume needs a session".to_string();
            self.toast("Session name required", ToastKind::Warning);
            return;
        }

        self.persist_session();

        let Some(session) = self.session.as_mut() else {
            self.status_line = "session storage disabled".to_string();
            self.toast("No session storage", ToastKind::Warning);
            return;
        };

        match session.switch_to(session_id) {
            Ok(transcript) => {
                let name = session.current_id();
                self.transcript = transcript;
                self.context_engine.reset();
                self.last_compaction = None;
                self.touch_transcript();
                self.selected_tool = None;
                self.streaming_message = None;
                self.scroll_chat_to_bottom();
                self.status_line = format!("resumed {name}");
                self.toast("Session resumed", ToastKind::Success);
            }
            Err(error) => {
                self.status_line = format!("resume failed: {error}");
                self.toast("Resume failed", ToastKind::Error);
            }
        }
    }

    pub(super) fn fork_session(&mut self) {
        if self.model_events.is_some() {
            self.status_line = "finish current turn before forking".to_string();
            self.toast("Cannot fork while working", ToastKind::Warning);
            return;
        }

        let Some(session) = self.session.as_mut() else {
            self.status_line = "session storage disabled".to_string();
            self.toast("No session to fork", ToastKind::Warning);
            return;
        };

        match session.fork(&self.transcript) {
            Ok(name) => {
                self.selected_tool = None;
                self.status_line = format!("forked {name}");
                self.toast("Session forked", ToastKind::Success);
            }
            Err(error) => {
                self.status_line = format!("fork failed: {error}");
                self.toast("Fork failed", ToastKind::Error);
            }
        }
    }

    pub(super) fn has_running_background_jobs(&self) -> bool {
        self.background_jobs
            .values()
            .any(|job| job.state == ToolRunState::Running)
    }

    /// A rewind touches the same files a running turn or background job may
    /// be writing; refuse instead of racing them.
    pub(super) fn rewind_blocked_reason(&self) -> Option<&'static str> {
        if self.is_working() || self.has_active_workflows() {
            Some("finish the current turn before rewinding")
        } else if self.has_running_background_jobs() {
            Some("stop background jobs before rewinding")
        } else {
            None
        }
    }

    pub(super) fn open_rewind_modal(&mut self) {
        if let Some(reason) = self.rewind_blocked_reason() {
            self.status_line = reason.to_string();
            self.toast("Cannot rewind now", ToastKind::Warning);
            return;
        }

        let entries = CheckpointStore::open(self.tools.workspace())
            .and_then(|store| store.list())
            .unwrap_or_default();
        if entries.is_empty() {
            self.status_line = "no checkpoints yet".to_string();
            self.toast("No checkpoints to rewind to", ToastKind::Info);
            return;
        }

        self.rewind_entries = entries;
        self.rewind_selection = 0;
        self.rewind_stage = RewindStage::Pick;
        self.rewind_confirm_selection = 0;
        self.active_modal = Some(Modal::Rewind);
        self.status_line = "rewind opened".to_string();
    }

    pub(super) fn selected_rewind_entry(&self) -> Option<&CheckpointEntry> {
        self.rewind_entries.get(self.rewind_selection)
    }

    /// Fork is only offered for checkpoints from the current session; grafting
    /// a foreign transcript onto this session would be nonsense.
    ///
    /// Session-id equality is necessary but NOT sufficient: `/clear` empties
    /// the transcript without rotating the session id, so a pre-clear
    /// checkpoint's `transcript_user_index` no longer maps to its user message.
    /// Forking on that stale index truncates the live transcript at a
    /// meaningless row. So also require the recorded index to still point at a
    /// live user message whose prompt matches the checkpoint's excerpt; if it
    /// does not, only file-only restore is offered.
    pub(super) fn selected_rewind_offers_fork(&self) -> bool {
        match (self.selected_rewind_entry(), self.session.as_ref()) {
            (Some(entry), Some(session)) => {
                entry.session_id == session.current_id() && self.checkpoint_index_is_live(entry)
            }
            _ => false,
        }
    }

    /// True when the checkpoint's recorded user-message row still exists in the
    /// current transcript, is a user message, and its prompt still matches the
    /// checkpoint's excerpt — i.e. forking at that index would land on the
    /// prompt the checkpoint was taken for, not a post-`/clear` coincidence.
    pub(super) fn checkpoint_index_is_live(&self, entry: &CheckpointEntry) -> bool {
        matches!(
            self.transcript.get(entry.transcript_user_index),
            Some(TranscriptItem::Message(message))
                if message.role == ChatRole::User
                    && excerpt_for_checkpoint(&message.content) == entry.prompt_excerpt
        )
    }

    /// Confirm-screen options: file-only restore is the default; fork is an
    /// extra option when available; cancel is always last.
    pub(super) fn rewind_confirm_options(&self) -> Vec<&'static str> {
        if self.selected_rewind_offers_fork() {
            vec![
                "Restore files",
                "Restore files + fork conversation",
                "Cancel",
            ]
        } else {
            vec!["Restore files", "Cancel"]
        }
    }

    pub(super) fn handle_rewind_key(&mut self, key: KeyEvent) {
        match self.rewind_stage {
            RewindStage::Pick => match key.code {
                KeyCode::Esc => self.close_modal(),
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.should_quit = true;
                }
                KeyCode::Up | KeyCode::BackTab => {
                    self.rewind_selection = self.rewind_selection.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Tab => {
                    self.rewind_selection = (self.rewind_selection + 1)
                        .min(self.rewind_entries.len().saturating_sub(1));
                }
                KeyCode::Home => self.rewind_selection = 0,
                KeyCode::End => {
                    self.rewind_selection = self.rewind_entries.len().saturating_sub(1);
                }
                KeyCode::Enter => {
                    if self.selected_rewind_entry().is_some() {
                        self.rewind_stage = RewindStage::Confirm;
                        self.rewind_confirm_selection = 0;
                    }
                }
                _ => {}
            },
            RewindStage::Confirm => match key.code {
                KeyCode::Esc => {
                    self.rewind_stage = RewindStage::Pick;
                }
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.should_quit = true;
                }
                KeyCode::Up | KeyCode::BackTab => {
                    self.rewind_confirm_selection = self.rewind_confirm_selection.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Tab => {
                    self.rewind_confirm_selection = (self.rewind_confirm_selection + 1)
                        .min(self.rewind_confirm_options().len().saturating_sub(1));
                }
                KeyCode::Enter => self.accept_rewind_confirm(),
                _ => {}
            },
        }
    }

    pub(super) fn accept_rewind_confirm(&mut self) {
        let options = self.rewind_confirm_options();
        let choice = options
            .get(self.rewind_confirm_selection)
            .copied()
            .unwrap_or("Cancel");
        match choice {
            "Restore files" => self.execute_rewind_restore(false),
            "Restore files + fork conversation" => self.execute_rewind_restore(true),
            _ => {
                self.rewind_stage = RewindStage::Pick;
            }
        }
    }

    pub(super) fn execute_rewind_restore(&mut self, fork: bool) {
        if let Some(reason) = self.rewind_blocked_reason() {
            self.status_line = reason.to_string();
            self.toast("Cannot rewind now", ToastKind::Warning);
            return;
        }
        let Some(entry) = self.selected_rewind_entry().cloned() else {
            self.close_modal();
            return;
        };

        let report = match CheckpointStore::open(self.tools.workspace())
            .and_then(|store| store.restore(&entry.id))
        {
            Ok(report) => report,
            Err(error) => {
                self.close_modal();
                self.status_line = format!("rewind failed: {error}");
                self.toast(format!("Rewind failed: {error}"), ToastKind::Error);
                return;
            }
        };

        self.close_modal();
        let rewound = report.restored.len() + report.deleted.len();
        let mut message = format!(
            "Rewound {rewound} file{}",
            if rewound == 1 { "" } else { "s" }
        );
        if !report.skipped.is_empty() {
            message.push_str(&format!(
                " · {} not rewindable (too large or symlink)",
                report.skipped.len()
            ));
        }
        if !report.refused.is_empty() {
            message.push_str(&format!(
                " · {} refused (would escape workspace)",
                report.refused.len()
            ));
        }
        let toast_kind = if report.refused.is_empty() {
            ToastKind::Success
        } else {
            ToastKind::Error
        };
        self.toast(message, toast_kind);
        self.status_line = format!("rewound to before {}", truncate(&entry.prompt_excerpt, 48));

        if fork {
            self.fork_transcript_at_checkpoint(&entry);
        }
    }

    /// Fork the conversation back to the checkpoint's turn: drop that user
    /// message and everything after it, fork the session file, and put the
    /// old prompt back in the composer for editing.
    ///
    /// Refuses (leaving files-only restore intact) when the checkpoint's
    /// recorded index no longer maps to its user message — e.g. after `/clear`
    /// invalidated all indices without rotating the session id. Forking on a
    /// stale index would truncate the live transcript at a meaningless row.
    pub(super) fn fork_transcript_at_checkpoint(&mut self, entry: &CheckpointEntry) {
        if !self.checkpoint_index_is_live(entry) {
            self.status_line = "fork skipped: checkpoint predates the current conversation".into();
            self.toast(
                "Files restored; conversation left as-is (checkpoint is from a cleared timeline)",
                ToastKind::Warning,
            );
            return;
        }
        if let Some(name) = self.fork_transcript_before(entry.transcript_user_index) {
            self.status_line = format!("forked {name}");
            self.toast("Conversation forked at checkpoint", ToastKind::Success);
        }
    }

    /// Shared backtrack core: truncate the live transcript to just before
    /// the user message at `index`, fork the session file so the original
    /// timeline stays reachable via /tree, and put the dropped prompt back
    /// in the composer for editing. Returns the forked session name.
    pub(super) fn fork_transcript_before(&mut self, index: usize) -> Option<String> {
        let Some(session) = self.session.as_mut() else {
            self.toast("No session to fork", ToastKind::Warning);
            return None;
        };

        let index = index.min(self.transcript.len());
        let old_prompt = match self.transcript.get(index) {
            Some(TranscriptItem::Message(message)) if message.role == ChatRole::User => {
                message.content.clone()
            }
            _ => String::new(),
        };

        self.transcript.truncate(index);
        match session.fork(&self.transcript) {
            Ok(name) => {
                self.touch_transcript();
                self.selected_tool = None;
                self.streaming_message = None;
                self.context_engine.reset();
                self.last_compaction = None;
                self.scroll_chat_to_bottom();
                self.input = old_prompt;
                self.input_cursor = self.input_len();
                Some(name)
            }
            Err(error) => {
                self.status_line = format!("fork failed: {error}");
                self.toast("Fork failed", ToastKind::Error);
                None
            }
        }
    }

    /// `/edit`: open the backtrack picker over previous user messages.
    /// Double-Esc on an idle composer is already the quit gesture, so
    /// backtracking lives on a slash command instead of a key chord.
    pub(super) fn open_edit_message_modal(&mut self) {
        if self.is_working() {
            self.status_line = "finish the current turn before editing a message".to_string();
            self.toast("Cannot edit while a turn is running", ToastKind::Warning);
            return;
        }
        let entries = self
            .transcript
            .iter()
            .enumerate()
            .rev()
            .filter_map(|(index, item)| match item {
                TranscriptItem::Message(message) if message.role == ChatRole::User => {
                    Some(EditPickerEntry {
                        transcript_index: index,
                        preview: message_one_liner(&message.content, 60),
                    })
                }
                _ => None,
            })
            .take(EDIT_PICKER_LIMIT)
            .collect::<Vec<_>>();
        if entries.is_empty() {
            self.status_line = "no previous messages to edit".to_string();
            self.toast("Nothing to edit yet", ToastKind::Info);
            return;
        }
        self.edit_picker_entries = entries;
        self.edit_picker_selection = 0;
        self.active_modal = Some(Modal::EditMessage);
        self.status_line = "pick a message to edit".to_string();
    }

    pub(super) fn handle_edit_message_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.close_modal(),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true;
            }
            KeyCode::Up | KeyCode::BackTab => {
                self.edit_picker_selection = self.edit_picker_selection.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Tab => {
                self.edit_picker_selection = (self.edit_picker_selection + 1)
                    .min(self.edit_picker_entries.len().saturating_sub(1));
            }
            KeyCode::Home => self.edit_picker_selection = 0,
            KeyCode::End => {
                self.edit_picker_selection = self.edit_picker_entries.len().saturating_sub(1);
            }
            KeyCode::Enter => self.accept_edit_message_selection(),
            _ => {}
        }
    }

    pub(super) fn accept_edit_message_selection(&mut self) {
        let Some(index) = self
            .edit_picker_entries
            .get(self.edit_picker_selection)
            .map(|entry| entry.transcript_index)
        else {
            self.close_modal();
            return;
        };
        self.active_modal = None;
        if self.is_working() {
            self.status_line = "finish the current turn before editing a message".to_string();
            self.toast("Cannot edit while a turn is running", ToastKind::Warning);
            return;
        }
        if self.fork_transcript_before(index).is_some() {
            self.status_line =
                "editing message — enter resends from here (original timeline kept in /tree)"
                    .to_string();
            self.toast(
                "Backtracked — original timeline kept in /tree",
                ToastKind::Success,
            );
        }
    }

    /// `/review`: seed the composer with a code-review prompt (never
    /// auto-sent, so scope can be trimmed first). Toasts instead when the
    /// workspace has no git repo or no pending changes.
    pub(super) fn run_review_command(&mut self) {
        if !(self.review_diff_check)(self.tools.workspace()) {
            self.status_line = "nothing to review".to_string();
            self.toast("Nothing to review", ToastKind::Info);
            return;
        }
        self.input = REVIEW_PROMPT_TEMPLATE.to_string();
        self.input_cursor = self.input_len();
        self.status_line = "review prompt ready — edit and press enter".to_string();
    }
}
