use super::*;

impl App {
    pub(super) fn handle_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }

        // Approval prompts take priority over every other surface: a worker
        // thread is blocked waiting on this answer.
        if !self.approval_queue.is_empty() {
            // Ignore (but consume) keystrokes for a brief window after the
            // prompt appears so an in-flight keypress can't blindly decide.
            if self
                .approval_shown_at
                .is_none_or(|shown| shown.elapsed() < APPROVAL_KEY_GRACE)
            {
                if self.approval_shown_at.is_none() {
                    self.approval_shown_at = Some(Instant::now());
                }
                return;
            }
            if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                self.should_quit = true;
                return;
            }
            // Decision keys must be unmodified: Ctrl+A (readline home) and the
            // like must never approve or persist a grant.
            let plain = key.modifiers.difference(KeyModifiers::SHIFT).is_empty();
            if plain {
                match key.code {
                    KeyCode::Char('y') | KeyCode::Char('Y') => {
                        self.resolve_pending_approval(ApprovalDecision::AllowOnce);
                    }
                    KeyCode::Char('a') | KeyCode::Char('A') => {
                        // Escalation cards do not offer always-allow: a
                        // persisted grant must never silently unsandbox
                        // future runs.
                        if self
                            .approval_queue
                            .front()
                            .is_none_or(|pending| !pending.request.sandbox_escalation)
                        {
                            self.resolve_pending_approval(ApprovalDecision::AlwaysAllow);
                        }
                    }
                    KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                        self.last_escape_at = None;
                        self.resolve_pending_approval(ApprovalDecision::Deny);
                    }
                    _ => {}
                }
            }
            return;
        }

        if self.active_modal.is_some() {
            if self.active_modal == Some(Modal::ImagePreview) {
                match key.code {
                    KeyCode::Esc => self.close_modal(),
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        self.should_quit = true;
                    }
                    KeyCode::Char('j') | KeyCode::Down | KeyCode::Right | KeyCode::Tab => {
                        self.move_image_preview_next();
                    }
                    KeyCode::Char('k') | KeyCode::Up | KeyCode::Left | KeyCode::BackTab => {
                        self.move_image_preview_previous();
                    }
                    KeyCode::Home => self.move_image_preview_first(),
                    KeyCode::End => self.move_image_preview_last(),
                    KeyCode::Char('+') | KeyCode::Char('=') => self.zoom_image_preview_in(),
                    KeyCode::Char('-') => self.zoom_image_preview_out(),
                    KeyCode::Char('0') => self.reset_image_preview_zoom(),
                    KeyCode::Char('o') => self.open_selected_preview_image_external(),
                    KeyCode::Char('y') => self.copy_selected_preview_image_path(),
                    KeyCode::Char('d') | KeyCode::Delete | KeyCode::Backspace => {
                        self.detach_current_preview_image();
                    }
                    _ => {}
                }
                return;
            }

            if self.active_modal == Some(Modal::Jobs) {
                match key.code {
                    KeyCode::Esc => self.close_modal(),
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        self.should_quit = true
                    }
                    _ => {}
                }
                return;
            }

            if self.active_modal == Some(Modal::Themes) {
                match key.code {
                    KeyCode::Esc => self.close_modal(),
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        self.should_quit = true;
                    }
                    KeyCode::Up | KeyCode::BackTab => self.move_theme_selection_up(),
                    KeyCode::Down | KeyCode::Tab => self.move_theme_selection_down(),
                    KeyCode::Home => {
                        self.theme_selection = 0;
                        self.preview_theme_selection();
                    }
                    KeyCode::End => {
                        self.theme_selection = ThemeKind::all().len().saturating_sub(1);
                        self.preview_theme_selection();
                    }
                    KeyCode::Enter => self.accept_theme_selection(),
                    _ => {}
                }
                return;
            }

            if self.active_modal == Some(Modal::Rewind) {
                self.handle_rewind_key(key);
                return;
            }

            if self.active_modal == Some(Modal::EditMessage) {
                self.handle_edit_message_key(key);
                return;
            }

            if self.active_modal == Some(Modal::Models) {
                match key.code {
                    KeyCode::Esc => self.close_modal(),
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        self.should_quit = true;
                    }
                    KeyCode::Left | KeyCode::Char('h') | KeyCode::BackTab => {
                        self.model_picker_pane = ModelPickerPane::Models;
                        self.status_line = "choose model".to_string();
                    }
                    KeyCode::Right | KeyCode::Char('l') | KeyCode::Tab => {
                        self.model_picker_pane = ModelPickerPane::Reasoning;
                        self.status_line = "choose reasoning effort".to_string();
                    }
                    KeyCode::Up => self.move_model_picker_selection_up(),
                    KeyCode::Down => self.move_model_picker_selection_down(),
                    KeyCode::Home => self.move_model_picker_selection_home(),
                    KeyCode::End => self.move_model_picker_selection_end(),
                    KeyCode::Enter => self.accept_model_picker_selection(),
                    _ => {}
                }
                return;
            }

            if self.active_modal == Some(Modal::Reasoning) {
                match key.code {
                    KeyCode::Esc => self.close_modal(),
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        self.should_quit = true;
                    }
                    KeyCode::Up | KeyCode::BackTab => self.move_reasoning_selection_up(),
                    KeyCode::Down | KeyCode::Tab => self.move_reasoning_selection_down(),
                    KeyCode::Home => self.reasoning_selection = 0,
                    KeyCode::End => {
                        self.reasoning_selection = reasoning_choices(
                            self.model.model_name(),
                            self.model.reasoning_effort(),
                        )
                        .len()
                        .saturating_sub(1);
                    }
                    KeyCode::Enter => self.accept_reasoning_selection(),
                    _ => {}
                }
                return;
            }

            if self.active_modal == Some(Modal::Permissions) {
                match key.code {
                    KeyCode::Esc => self.close_modal(),
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        self.should_quit = true;
                    }
                    KeyCode::Up | KeyCode::BackTab => self.move_permission_selection_up(),
                    KeyCode::Down | KeyCode::Tab => self.move_permission_selection_down(),
                    KeyCode::Home => self.permission_selection = 0,
                    KeyCode::End => {
                        self.permission_selection = PermissionMode::all().len().saturating_sub(1);
                    }
                    KeyCode::Enter => self.accept_permission_selection(),
                    _ => {}
                }
                return;
            }

            if self.active_modal == Some(Modal::Settings) {
                match key.code {
                    KeyCode::Esc => self.close_modal(),
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        self.should_quit = true;
                    }
                    KeyCode::Up | KeyCode::BackTab => self.move_settings_selection_up(),
                    KeyCode::Down | KeyCode::Tab => self.move_settings_selection_down(),
                    KeyCode::Home => self.settings_selection = 0,
                    KeyCode::End => {
                        self.settings_selection = self.settings_rows().len().saturating_sub(1);
                    }
                    KeyCode::Enter => self.accept_settings_selection(),
                    _ => {}
                }
                return;
            }

            match key.code {
                KeyCode::Esc | KeyCode::Enter => self.close_modal(),
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.should_quit = true;
                }
                _ => {}
            }
            return;
        }

        match key.code {
            KeyCode::Esc if self.slash_suggestions_active() => self.close_command_palette(),
            KeyCode::Esc if self.mention_popup_visible() => self.dismiss_mention_picker(),
            KeyCode::Esc => self.handle_escape(),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true;
            }
            KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.open_command_palette();
            }
            KeyCode::Char('i') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.paste_image_from_clipboard();
            }
            KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.open_latest_image_preview();
            }
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.detach_latest_pending_attachment();
            }
            KeyCode::PageUp if self.slash_suggestions_active() => self.page_slash_selection_up(),
            KeyCode::PageDown if self.slash_suggestions_active() => {
                self.page_slash_selection_down();
            }
            KeyCode::PageUp => self.scroll_chat_up(self.chat_page_scroll_amount()),
            KeyCode::PageDown => self.scroll_chat_down(self.chat_page_scroll_amount()),
            KeyCode::Up if key.modifiers.contains(KeyModifiers::CONTROL) => self.scroll_chat_up(1),
            KeyCode::Down if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.scroll_chat_down(1);
            }
            KeyCode::Home if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.scroll_chat_to_top();
            }
            KeyCode::End if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.scroll_chat_to_bottom();
            }
            _ if self.handle_decision_key(key) => {}
            KeyCode::Up if self.slash_suggestions_active() => self.move_slash_selection_up(),
            KeyCode::Down if self.slash_suggestions_active() => self.move_slash_selection_down(),
            KeyCode::Tab if self.slash_suggestions_active() => self.move_slash_selection_down(),
            KeyCode::BackTab if self.slash_suggestions_active() => self.move_slash_selection_up(),
            KeyCode::Up if self.mention_popup_visible() => self.move_mention_selection_up(),
            KeyCode::Down if self.mention_popup_visible() => self.move_mention_selection_down(),
            KeyCode::Tab if self.mention_popup_visible() => self.accept_mention_suggestion(),
            KeyCode::BackTab if self.mention_popup_visible() => self.move_mention_selection_up(),
            KeyCode::BackTab => self.toggle_plan_mode(),
            KeyCode::Home if key.modifiers.is_empty() && self.slash_suggestions_active() => {
                self.move_slash_selection_first();
            }
            KeyCode::End if key.modifiers.is_empty() && self.slash_suggestions_active() => {
                self.move_slash_selection_last();
            }
            KeyCode::Char('j') if self.input.is_empty() && self.pending_decision().is_none() => {
                self.select_next_tool()
            }
            KeyCode::Char('k') if self.input.is_empty() && self.pending_decision().is_none() => {
                self.select_previous_tool()
            }
            KeyCode::Char('x') if self.input.is_empty() && self.pending_decision().is_none() => {
                self.close_selected_tool()
            }
            KeyCode::Enter if self.input.is_empty() && self.selected_tool.is_some() => {
                self.toggle_selected_tool();
            }
            KeyCode::Enter
                if key
                    .modifiers
                    .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
            {
                self.insert_input_char('\n');
            }
            KeyCode::Enter | KeyCode::Char('\n') | KeyCode::Char('\r')
                if self.slash_suggestions_active() =>
            {
                self.accept_slash_suggestion();
            }
            KeyCode::Enter | KeyCode::Char('\n') | KeyCode::Char('\r')
                if self.mention_popup_visible() =>
            {
                self.accept_mention_suggestion();
            }
            KeyCode::Enter | KeyCode::Char('\n') | KeyCode::Char('\r') => self.submit_input(),
            KeyCode::Char('j' | 'm') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.submit_input();
            }
            KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.input_cursor = self.input_len();
            }
            KeyCode::Left => self.move_input_cursor_left(),
            KeyCode::Right => self.move_input_cursor_right(),
            KeyCode::Up if !self.input.is_empty() && self.input.contains('\n') => {
                self.move_input_cursor_vertical(-1);
            }
            KeyCode::Down if !self.input.is_empty() && self.input.contains('\n') => {
                self.move_input_cursor_vertical(1);
            }
            KeyCode::Home if key.modifiers.is_empty() => {
                self.input_cursor = self.input_current_line_bounds().0;
            }
            KeyCode::End if key.modifiers.is_empty() => {
                self.input_cursor = self.input_current_line_bounds().1;
            }
            KeyCode::Delete => self.delete_input_char(),
            KeyCode::Char(_)
                if key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.status_line = "Unsupported shortcut.".to_string();
            }
            KeyCode::Char(ch) => self.insert_input_char(ch),
            KeyCode::Backspace => self.backspace_input_char(),
            _ => {}
        }
    }

    pub(super) fn input_len(&self) -> usize {
        self.input.chars().count()
    }

    /// Char-index bounds (start, end) of the line the cursor is on,
    /// excluding the trailing newline.
    pub(super) fn input_current_line_bounds(&self) -> (usize, usize) {
        let mut start = 0;
        let mut index = 0;
        for ch in self.input.chars() {
            if ch == '\n' {
                if index >= self.input_cursor {
                    return (start, index);
                }
                start = index + 1;
            }
            index += 1;
        }
        (start, index)
    }

    /// Move the cursor up/down one visual input line, keeping the column
    /// where possible (clamped to the target line's length).
    pub(super) fn move_input_cursor_vertical(&mut self, delta: isize) {
        let lines: Vec<&str> = self.input.split('\n').collect();
        // Locate the cursor's (line, column).
        let mut remaining = self.input_cursor;
        let mut line_index = 0;
        for (index, line) in lines.iter().enumerate() {
            let len = line.chars().count();
            if remaining <= len {
                line_index = index;
                break;
            }
            remaining -= len + 1;
            line_index = index;
        }
        let column = remaining;

        let target = line_index.saturating_add_signed(delta);
        if target >= lines.len() || target == line_index {
            return;
        }

        let target_column = column.min(lines[target].chars().count());
        let mut cursor = 0;
        for line in lines.iter().take(target) {
            cursor += line.chars().count() + 1;
        }
        self.input_cursor = cursor + target_column;
    }

    pub(super) fn input_byte_index(&self, char_index: usize) -> usize {
        self.input
            .char_indices()
            .nth(char_index)
            .map(|(index, _)| index)
            .unwrap_or(self.input.len())
    }

    pub(super) fn insert_input_char(&mut self, ch: char) {
        let index = self.input_byte_index(self.input_cursor);
        self.input.insert(index, ch);
        self.input_cursor += 1;
        self.clamp_slash_selection();
        self.refresh_mention_state();
    }

    pub(super) fn backspace_input_char(&mut self) {
        if self.input_cursor == 0 {
            self.clamp_slash_selection();
            return;
        }

        let start = self.input_byte_index(self.input_cursor - 1);
        let end = self.input_byte_index(self.input_cursor);
        self.input.replace_range(start..end, "");
        self.input_cursor -= 1;
        self.clamp_slash_selection();
        self.refresh_mention_state();
    }

    pub(super) fn delete_input_char(&mut self) {
        if self.input_cursor >= self.input_len() {
            self.clamp_slash_selection();
            return;
        }

        let start = self.input_byte_index(self.input_cursor);
        let end = self.input_byte_index(self.input_cursor + 1);
        self.input.replace_range(start..end, "");
        self.clamp_slash_selection();
        self.refresh_mention_state();
    }

    pub(super) fn move_input_cursor_left(&mut self) {
        self.input_cursor = self.input_cursor.saturating_sub(1);
    }

    pub(super) fn move_input_cursor_right(&mut self) {
        self.input_cursor = (self.input_cursor + 1).min(self.input_len());
    }
}
