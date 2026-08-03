use super::*;

impl App {
    pub(super) fn open_command_palette(&mut self) {
        self.input = "/".to_string();
        self.input_cursor = self.input_len();
        self.slash_selection = 0;
        self.status_line = "command palette".to_string();
    }

    pub(super) fn close_command_palette(&mut self) {
        self.input.clear();
        self.input_cursor = 0;
        self.slash_selection = 0;
        self.status_line = "command palette closed".to_string();
    }

    /// Contextual Esc: clear what's in the way first; only a second quick
    /// Esc on an idle composer quits, so a stray keypress can't kill the
    /// session.
    pub(super) fn handle_escape(&mut self) {
        if self.selected_tool.is_some() {
            self.close_selected_tool();
            return;
        }
        if !self.input.is_empty() {
            self.input.clear();
            self.input_cursor = 0;
            self.last_escape_at = None;
            self.status_line = "input cleared".to_string();
            return;
        }
        if self.is_working() {
            // Never arms double-esc quit: cancelling a turn and quitting the
            // app must stay two distinct gestures.
            self.last_escape_at = None;
            if self.cancel_requested_at.is_some() {
                self.force_abandon_turn();
            } else {
                self.request_cancel_turn();
            }
            return;
        }
        if self.has_active_workflows() {
            // A background workflow is running but no model turn is streaming
            // (`is_working()` is false). Esc cancels the workflow — never falls
            // through to the double-esc quit arm, which would kill subagents
            // mid file_edit/file_patch and orphan their process-grouped
            // children. Like a turn cancel, this never arms quit.
            self.last_escape_at = None;
            self.cancel_active_workflows();
            return;
        }
        if self.plan_mode {
            self.toggle_plan_mode();
            self.last_escape_at = None;
            return;
        }
        if self
            .last_escape_at
            .is_some_and(|at| at.elapsed() <= DOUBLE_ESCAPE_WINDOW)
        {
            self.should_quit = true;
            return;
        }
        self.last_escape_at = Some(Instant::now());
        self.status_line = "press esc again to quit".to_string();
    }

    pub(super) fn toggle_plan_mode(&mut self) {
        self.plan_mode = !self.plan_mode;
        if self.plan_mode {
            self.status_line = "plan mode on · read-only exploration".to_string();
            self.toast(
                "Plan mode on — model will plan before editing",
                ToastKind::Info,
            );
        } else {
            self.status_line = "plan mode off".to_string();
            self.toast("Plan mode off — edits allowed", ToastKind::Info);
        }
    }

    pub(super) fn close_modal(&mut self) {
        if self.active_modal == Some(Modal::Themes) {
            self.cancel_theme_preview();
        }
        self.active_modal = None;
        self.status_line = "closed".to_string();
    }

    pub(super) fn slash_suggestions_active(&self) -> bool {
        let input = self.input.trim_start();
        input.starts_with('/')
            && (!input.contains(char::is_whitespace) || input.starts_with("/theme "))
    }

    pub(super) fn slash_matches(&self) -> Vec<(&'static SlashCommand, Vec<usize>)> {
        if !self.slash_suggestions_active() {
            return Vec::new();
        }

        let input = self.input.trim_start();
        let (commands, query) = if let Some(theme_query) = input.strip_prefix("/theme ") {
            (
                THEME_SLASH_COMMANDS,
                theme_query.trim().to_ascii_lowercase(),
            )
        } else {
            (
                SLASH_COMMANDS,
                input.trim_start_matches('/').trim().to_ascii_lowercase(),
            )
        };
        let mut matches = commands
            .iter()
            .enumerate()
            .filter_map(|(index, command)| {
                slash_match(command, &query)
                    .map(|(score, positions)| (score, index, command, positions))
            })
            .collect::<Vec<_>>();
        matches.sort_by_key(|(score, index, _, _)| (*score, *index));
        matches
            .into_iter()
            .map(|(_, _, command, positions)| (command, positions))
            .collect()
    }

    pub(super) fn clamp_slash_selection(&mut self) {
        let count = self.slash_matches().len();
        if count == 0 {
            self.slash_selection = 0;
        } else if self.slash_selection >= count {
            self.slash_selection = count - 1;
        }
    }

    pub(super) fn move_slash_selection_up(&mut self) {
        let count = self.slash_matches().len();
        if count == 0 {
            return;
        }
        self.slash_selection = if self.slash_selection == 0 {
            count - 1
        } else {
            self.slash_selection - 1
        };
        self.status_line = "command suggestion".to_string();
    }

    pub(super) fn move_slash_selection_down(&mut self) {
        let count = self.slash_matches().len();
        if count == 0 {
            return;
        }
        self.slash_selection = (self.slash_selection + 1) % count;
        self.status_line = "command suggestion".to_string();
    }

    pub(super) fn page_slash_selection_up(&mut self) {
        self.move_slash_selection_by(-6);
    }

    pub(super) fn page_slash_selection_down(&mut self) {
        self.move_slash_selection_by(6);
    }

    pub(super) fn move_slash_selection_by(&mut self, amount: isize) {
        let count = self.slash_matches().len();
        if count == 0 {
            return;
        }

        let max = count.saturating_sub(1) as isize;
        let next = (self.slash_selection as isize + amount).clamp(0, max);
        self.slash_selection = next as usize;
        self.status_line = "command suggestion".to_string();
    }

    pub(super) fn move_slash_selection_first(&mut self) {
        if !self.slash_matches().is_empty() {
            self.slash_selection = 0;
            self.status_line = "command suggestion".to_string();
        }
    }

    pub(super) fn move_slash_selection_last(&mut self) {
        let count = self.slash_matches().len();
        if count > 0 {
            self.slash_selection = count - 1;
            self.status_line = "command suggestion".to_string();
        }
    }

    pub(super) fn accept_slash_suggestion(&mut self) {
        let Some(command) = self
            .slash_matches()
            .get(self.slash_selection)
            .map(|(command, _)| *command)
        else {
            self.submit_input();
            return;
        };

        // Typing a command out in full and hitting Enter runs it as typed
        // instead of re-completing it into the composer.
        if self.input.trim() == command.name
            && !matches!(
                command.name,
                "/theme" | "/model" | "/reasoning" | "/permissions"
            )
        {
            self.submit_input();
            return;
        }

        if command.name == "/theme" {
            self.input.clear();
            self.input_cursor = 0;
            self.open_themes_modal();
        } else if command.name == "/model" {
            self.input.clear();
            self.input_cursor = 0;
            self.open_models_modal();
        } else if command.name == "/reasoning" {
            self.input.clear();
            self.input_cursor = 0;
            self.open_reasoning_modal();
        } else if command.name == "/permissions" {
            self.input.clear();
            self.input_cursor = 0;
            self.open_permissions_modal();
        } else if command.args.is_empty() {
            self.input = command.name.to_string();
            self.input_cursor = self.input_len();
            self.submit_input();
        } else {
            self.input = format!("{} ", command.name);
            self.input_cursor = self.input_len();
            self.status_line = format!("{} needs {}", command.name, command.args);
        }
    }

    /// Char-index span (start..end) and typed query of the @token under the
    /// cursor: an '@' at a token boundary (start of input or after
    /// whitespace) with no whitespace between it and the cursor. `end`
    /// extends to the end of the contiguous token so accepting a suggestion
    /// mid-token replaces the whole token; the query is only what was typed
    /// so far (between '@' and the cursor).
    pub(super) fn active_mention_token(&self) -> Option<(usize, usize, String)> {
        let chars: Vec<char> = self.input.chars().collect();
        let cursor = self.input_cursor.min(chars.len());
        let mut at = None;
        for index in (0..cursor).rev() {
            let ch = chars[index];
            if ch == '@' {
                if index == 0 || chars[index - 1].is_whitespace() {
                    at = Some(index);
                }
                break;
            }
            if ch.is_whitespace() {
                break;
            }
        }
        let start = at?;
        let mut end = cursor;
        while end < chars.len() && !chars[end].is_whitespace() {
            end += 1;
        }
        let query = chars[start + 1..cursor].iter().collect();
        Some((start, end, query))
    }

    pub(super) fn mention_active(&self) -> bool {
        !self.mention_dismissed
            && !self.slash_suggestions_active()
            && self.active_mention_token().is_some()
    }

    pub(super) fn mention_popup_visible(&self) -> bool {
        self.mention_active() && !self.mention_matches().is_empty()
    }

    /// Keep mention picker state in sync after any composer edit: load the
    /// workspace file list when an @token appears, drop it when the token
    /// goes away, and clear an Esc dismissal (typing reopens the picker).
    pub(super) fn refresh_mention_state(&mut self) {
        self.mention_dismissed = false;
        if !self.slash_suggestions_active() && self.active_mention_token().is_some() {
            if self.mention_files.is_none() {
                self.mention_files = Some(collect_workspace_files(
                    self.tools.workspace(),
                    MENTION_FILE_WALK_CAP,
                ));
            }
            self.clamp_mention_selection();
        } else {
            self.mention_files = None;
            self.mention_selection = 0;
        }
    }

    pub(super) fn mention_matches(&self) -> Vec<(&str, Vec<usize>)> {
        if !self.mention_active() {
            return Vec::new();
        }
        let Some((_, _, query)) = self.active_mention_token() else {
            return Vec::new();
        };
        let Some(files) = &self.mention_files else {
            return Vec::new();
        };

        let query = query.to_ascii_lowercase();
        let mut matches = files
            .iter()
            .enumerate()
            .filter_map(|(index, path)| {
                mention_match(path, &query)
                    .map(|(score, positions)| (score, index, path.as_str(), positions))
            })
            .collect::<Vec<_>>();
        matches.sort_by_key(|(score, index, _, _)| (*score, *index));
        matches.truncate(MENTION_MATCH_LIMIT);
        matches
            .into_iter()
            .map(|(_, _, path, positions)| (path, positions))
            .collect()
    }

    pub(super) fn clamp_mention_selection(&mut self) {
        let count = self.mention_matches().len();
        if count == 0 {
            self.mention_selection = 0;
        } else if self.mention_selection >= count {
            self.mention_selection = count - 1;
        }
    }

    pub(super) fn move_mention_selection_up(&mut self) {
        let count = self.mention_matches().len();
        if count == 0 {
            return;
        }
        self.mention_selection = if self.mention_selection == 0 {
            count - 1
        } else {
            self.mention_selection - 1
        };
        self.status_line = "file suggestion".to_string();
    }

    pub(super) fn move_mention_selection_down(&mut self) {
        let count = self.mention_matches().len();
        if count == 0 {
            return;
        }
        self.mention_selection = (self.mention_selection + 1) % count;
        self.status_line = "file suggestion".to_string();
    }

    /// Replace the @token with the selected workspace-relative path (plain
    /// text — the model reads files itself) plus a trailing space.
    pub(super) fn accept_mention_suggestion(&mut self) {
        let Some(path) = self
            .mention_matches()
            .get(self.mention_selection)
            .map(|(path, _)| (*path).to_string())
        else {
            return;
        };
        let Some((start, end, _)) = self.active_mention_token() else {
            return;
        };

        let start_byte = self.input_byte_index(start);
        let end_byte = self.input_byte_index(end);
        self.input
            .replace_range(start_byte..end_byte, &format!("{path} "));
        self.input_cursor = start + path.chars().count() + 1;
        self.mention_selection = 0;
        self.mention_files = None;
        self.status_line = format!("mentioned {path}");
    }

    pub(super) fn dismiss_mention_picker(&mut self) {
        self.mention_dismissed = true;
        self.mention_selection = 0;
        self.status_line = "file picker closed".to_string();
    }

    pub(super) fn open_settings_modal(&mut self) {
        self.active_modal = Some(Modal::Settings);
        self.settings_selection = 0;
        self.status_line = "settings opened".to_string();
    }

    pub(super) fn open_models_modal(&mut self) {
        self.active_modal = Some(Modal::Models);
        self.model_picker_pane = ModelPickerPane::Models;
        self.model_selection = model_index(self.model.model_name());
        self.sync_model_picker_reasoning_selection();
        self.status_line = "model and execution mode picker opened".to_string();
    }

    pub(super) fn open_reasoning_modal(&mut self) {
        self.active_modal = Some(Modal::Reasoning);
        self.reasoning_selection =
            reasoning_index(self.model.model_name(), self.model.reasoning_effort());
        self.status_line = "reasoning effort opened".to_string();
    }

    pub(super) fn open_permissions_modal(&mut self) {
        self.active_modal = Some(Modal::Permissions);
        self.permission_selection = permission_mode_index(self.permission_mode);
        self.status_line = "permissions opened".to_string();
    }

    pub(super) fn open_themes_modal(&mut self) {
        self.active_modal = Some(Modal::Themes);
        self.theme_preview_original.get_or_insert(self.theme);
        self.theme_selection = theme_index(self.theme);
        self.status_line = "themes opened".to_string();
    }

    pub(super) fn move_settings_selection_up(&mut self) {
        let count = self.settings_rows().len();
        if count == 0 {
            return;
        }
        self.settings_selection = if self.settings_selection == 0 {
            count - 1
        } else {
            self.settings_selection - 1
        };
        self.status_line = "settings selection".to_string();
    }

    pub(super) fn move_settings_selection_down(&mut self) {
        let count = self.settings_rows().len();
        if count == 0 {
            return;
        }
        self.settings_selection = (self.settings_selection + 1) % count;
        self.status_line = "settings selection".to_string();
    }

    pub(super) fn accept_settings_selection(&mut self) {
        let items = self.settings_items();
        match items.get(self.settings_selection).map(|item| item.key) {
            Some("model") => self.open_models_modal(),
            Some("reasoning") => self.open_reasoning_modal(),
            Some("thinking") => self.toggle_reasoning_visibility(),
            Some("theme") => self.open_themes_modal(),
            Some("permissions") => self.open_permissions_modal(),
            Some("bell") => self.toggle_bell_setting(),
            Some(_) | None => {
                self.status_line = "setting is read-only".to_string();
                self.toast("Read-only setting", ToastKind::Info);
            }
        }
    }

    pub(super) fn toggle_bell_setting(&mut self) {
        self.bell_setting = !self.bell_setting;
        let label = if self.bell_setting {
            "Bell on"
        } else {
            "Bell off"
        };
        match save_bell_preference(self.tools.workspace(), self.bell_setting) {
            Ok(()) => {
                self.status_line = label.to_ascii_lowercase();
                self.toast(label, ToastKind::Info);
            }
            Err(error) => {
                self.status_line = format!("bell preference not saved: {error}");
                self.toast("Bell preference not saved", ToastKind::Warning);
            }
        }
    }

    pub(super) fn toggle_reasoning_visibility(&mut self) {
        self.show_reasoning = !self.show_reasoning;
        self.touch_transcript();
        let label = if self.show_reasoning {
            "Thinking shown"
        } else {
            "Thinking hidden"
        };
        match save_reasoning_visibility(self.tools.workspace(), self.show_reasoning) {
            Ok(()) => {
                self.status_line = label.to_ascii_lowercase();
                self.toast(label, ToastKind::Info);
            }
            Err(error) => {
                self.status_line = format!("thinking preference not saved: {error}");
                self.toast("Thinking preference not saved", ToastKind::Warning);
            }
        }
    }

    pub(super) fn move_model_selection_up(&mut self) {
        let count = model_choices(self.model.model_name()).len();
        if count == 0 {
            return;
        }
        self.model_selection = if self.model_selection == 0 {
            count - 1
        } else {
            self.model_selection - 1
        };
        self.sync_model_picker_reasoning_selection();
        self.status_line = "choose model".to_string();
    }

    pub(super) fn move_model_selection_down(&mut self) {
        let count = model_choices(self.model.model_name()).len();
        if count == 0 {
            return;
        }
        self.model_selection = (self.model_selection + 1) % count;
        self.sync_model_picker_reasoning_selection();
        self.status_line = "choose model".to_string();
    }

    pub(super) fn selected_model_picker_model(&self) -> String {
        let choices = model_choices(self.model.model_name());
        choices
            .get(self.model_selection.min(choices.len().saturating_sub(1)))
            .cloned()
            .unwrap_or_else(|| self.model.model_name().to_string())
    }

    pub(super) fn model_picker_reasoning_choices(&self) -> Vec<String> {
        reasoning_choices(&self.selected_model_picker_model(), "")
    }

    pub(super) fn sync_model_picker_reasoning_selection(&mut self) {
        let model = self.selected_model_picker_model();
        let preferred = preferred_reasoning_for_model(&model, self.model.reasoning_effort());
        let choices = reasoning_choices(&model, "");
        self.reasoning_selection = choices
            .iter()
            .position(|effort| effort == &preferred)
            .unwrap_or(0);
    }

    pub(super) fn move_model_picker_selection_up(&mut self) {
        match self.model_picker_pane {
            ModelPickerPane::Models => self.move_model_selection_up(),
            ModelPickerPane::Reasoning => {
                let count = self.model_picker_reasoning_choices().len();
                if count == 0 {
                    return;
                }
                self.reasoning_selection = if self.reasoning_selection == 0 {
                    count - 1
                } else {
                    self.reasoning_selection - 1
                };
                self.status_line = "choose reasoning effort".to_string();
            }
        }
    }

    pub(super) fn move_model_picker_selection_down(&mut self) {
        match self.model_picker_pane {
            ModelPickerPane::Models => self.move_model_selection_down(),
            ModelPickerPane::Reasoning => {
                let count = self.model_picker_reasoning_choices().len();
                if count == 0 {
                    return;
                }
                self.reasoning_selection = (self.reasoning_selection + 1) % count;
                self.status_line = "choose reasoning effort".to_string();
            }
        }
    }

    pub(super) fn move_model_picker_selection_home(&mut self) {
        match self.model_picker_pane {
            ModelPickerPane::Models => {
                self.model_selection = 0;
                self.sync_model_picker_reasoning_selection();
                self.status_line = "choose model".to_string();
            }
            ModelPickerPane::Reasoning => {
                self.reasoning_selection = 0;
                self.status_line = "choose reasoning effort".to_string();
            }
        }
    }

    pub(super) fn move_model_picker_selection_end(&mut self) {
        match self.model_picker_pane {
            ModelPickerPane::Models => {
                self.model_selection = model_choices(self.model.model_name())
                    .len()
                    .saturating_sub(1);
                self.sync_model_picker_reasoning_selection();
                self.status_line = "choose model".to_string();
            }
            ModelPickerPane::Reasoning => {
                self.reasoning_selection = self
                    .model_picker_reasoning_choices()
                    .len()
                    .saturating_sub(1);
                self.status_line = "choose reasoning effort".to_string();
            }
        }
    }

    pub(super) fn accept_model_picker_selection(&mut self) {
        if self.model_picker_pane == ModelPickerPane::Models {
            self.model_picker_pane = ModelPickerPane::Reasoning;
            self.status_line = "choose reasoning effort".to_string();
            return;
        }

        let model = self.selected_model_picker_model();
        let choices = self.model_picker_reasoning_choices();
        let Some(effort) = choices
            .get(
                self.reasoning_selection
                    .min(choices.len().saturating_sub(1)),
            )
            .cloned()
        else {
            return;
        };

        if let Err(error) = self.model.try_set_model_name(model.clone()) {
            self.toast(format!("Model unavailable: {error}"), ToastKind::Error);
            self.status_line = "model unchanged".to_string();
            return;
        }
        self.model.set_reasoning_effort(effort.clone());
        self.context_engine
            .set_max_tokens(medusa_core::context::context_max_tokens_for_model(
                self.model.context_window(),
            ));
        self.model_selection = model_index(&model);
        self.reasoning_selection = reasoning_index(&model, &effort);
        self.active_modal = None;
        self.status_line = format!("model: {model} · reasoning: {effort}");
        match save_model_picker_preferences(self.tools.workspace(), &model, &effort) {
            Ok(()) => self.toast(
                format!("Model set to {model} · {effort}"),
                ToastKind::Success,
            ),
            Err(error) => self.toast(format!("Model set, save failed: {error}"), ToastKind::Error),
        }
    }

    pub(super) fn move_reasoning_selection_up(&mut self) {
        let count = reasoning_choices(self.model.model_name(), self.model.reasoning_effort()).len();
        if count == 0 {
            return;
        }
        self.reasoning_selection = if self.reasoning_selection == 0 {
            count - 1
        } else {
            self.reasoning_selection - 1
        };
        self.status_line = "reasoning selection".to_string();
    }

    pub(super) fn move_reasoning_selection_down(&mut self) {
        let count = reasoning_choices(self.model.model_name(), self.model.reasoning_effort()).len();
        if count == 0 {
            return;
        }
        self.reasoning_selection = (self.reasoning_selection + 1) % count;
        self.status_line = "reasoning selection".to_string();
    }

    pub(super) fn accept_reasoning_selection(&mut self) {
        let choices = reasoning_choices(self.model.model_name(), self.model.reasoning_effort());
        let Some(effort) = choices
            .get(
                self.reasoning_selection
                    .min(choices.len().saturating_sub(1)),
            )
            .cloned()
        else {
            return;
        };
        self.set_reasoning_effort(&effort);
        self.active_modal = None;
    }

    pub(super) fn move_permission_selection_up(&mut self) {
        let count = PermissionMode::all().len();
        if count == 0 {
            return;
        }
        self.permission_selection = if self.permission_selection == 0 {
            count - 1
        } else {
            self.permission_selection - 1
        };
        self.status_line = "permission selection".to_string();
    }

    pub(super) fn move_permission_selection_down(&mut self) {
        let count = PermissionMode::all().len();
        if count == 0 {
            return;
        }
        self.permission_selection = (self.permission_selection + 1) % count;
        self.status_line = "permission selection".to_string();
    }

    pub(super) fn accept_permission_selection(&mut self) {
        let mode = PermissionMode::all()[self
            .permission_selection
            .min(PermissionMode::all().len().saturating_sub(1))];
        self.set_permission_mode(mode);
        self.active_modal = None;
    }

    pub(super) fn move_theme_selection_up(&mut self) {
        let count = ThemeKind::all().len();
        self.theme_selection = if self.theme_selection == 0 {
            count - 1
        } else {
            self.theme_selection - 1
        };
        self.preview_theme_selection();
    }

    pub(super) fn move_theme_selection_down(&mut self) {
        let count = ThemeKind::all().len();
        self.theme_selection = (self.theme_selection + 1) % count;
        self.preview_theme_selection();
    }

    pub(super) fn preview_theme_selection(&mut self) {
        let theme = ThemeKind::all()[self.theme_selection.min(ThemeKind::all().len() - 1)];
        self.theme = theme;
        set_active_theme(theme);
        self.invalidate_render_cache();
        self.status_line = format!("preview theme: {}", theme.name());
    }

    pub(super) fn accept_theme_selection(&mut self) {
        let theme = ThemeKind::all()[self.theme_selection.min(ThemeKind::all().len() - 1)];
        self.theme_preview_original = None;
        self.set_theme(theme);
        self.active_modal = None;
    }

    pub(super) fn cancel_theme_preview(&mut self) {
        let Some(theme) = self.theme_preview_original.take() else {
            return;
        };
        self.theme = theme;
        self.theme_selection = theme_index(theme);
        set_active_theme(theme);
        self.invalidate_render_cache();
    }
}
