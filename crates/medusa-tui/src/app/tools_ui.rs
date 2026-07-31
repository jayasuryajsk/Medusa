use super::*;

impl App {
    pub(super) fn select_next_tool(&mut self) {
        let tools = self.tool_group_indices();
        if tools.is_empty() {
            self.status_line = "no tool activity".to_string();
            return;
        }

        self.selected_tool = Some(match self.selected_tool {
            Some(current) => tools
                .iter()
                .copied()
                .find(|index| *index > current)
                .unwrap_or(tools[0]),
            None => tools[0],
        });
        self.status_line = "tool selected".to_string();
    }

    pub(super) fn select_previous_tool(&mut self) {
        let tools = self.tool_group_indices();
        if tools.is_empty() {
            self.status_line = "no tool calls yet".to_string();
            return;
        }

        self.selected_tool = Some(match self.selected_tool {
            Some(current) => tools
                .iter()
                .rev()
                .copied()
                .find(|index| *index < current)
                .unwrap_or_else(|| *tools.last().unwrap()),
            None => *tools.last().unwrap(),
        });
        self.status_line = "tool selected".to_string();
    }

    pub(super) fn toggle_selected_tool(&mut self) {
        let Some(index) = self.selected_tool else {
            self.status_line = "no tool selected".to_string();
            return;
        };

        if let Some(TranscriptItem::Reasoning(trace)) = self.transcript.get_mut(index) {
            trace.expanded = !trace.expanded;
            let expanded = trace.expanded;
            self.touch_transcript();
            self.status_line = if expanded {
                "reasoning open".to_string()
            } else {
                "reasoning closed".to_string()
            };
            self.persist_session();
            return;
        }

        let Some((start, end)) = self.tool_group_range_containing(index) else {
            self.selected_tool = None;
            self.status_line = "no tool selected".to_string();
            return;
        };

        let coalescible = tool_group_has_coalesced_runs(&self.transcript, start, end);
        if coalescible && !tool_group_is_open(&self.transcript, start, end) {
            if let Some(run) = self.transcript[start..end]
                .iter_mut()
                .find_map(|item| match item {
                    TranscriptItem::Tool(run) => Some(run),
                    _ => None,
                })
            {
                run.group_expanded = true;
            }
            self.touch_transcript();
            self.selected_tool = Some(start);
            self.status_line = "tool group expanded".to_string();
            self.persist_session();
            return;
        }

        let next_expanded = !self.transcript[start..end]
            .iter()
            .any(|item| matches!(item, TranscriptItem::Tool(run) if run.expanded));

        for item in &mut self.transcript[start..end] {
            if let TranscriptItem::Tool(run) = item {
                run.expanded = false;
                if !next_expanded {
                    run.group_expanded = false;
                }
            }
        }

        if next_expanded
            && let Some(run) = self.transcript[start..end]
                .iter_mut()
                .find_map(|item| match item {
                    TranscriptItem::Tool(run) => Some(run),
                    _ => None,
                })
        {
            run.expanded = true;
        }

        self.touch_transcript();
        self.selected_tool = Some(start);
        self.status_line = if next_expanded {
            "tool details open".to_string()
        } else if coalescible {
            "tool group collapsed".to_string()
        } else {
            "tool details closed".to_string()
        };
        self.persist_session();
    }

    pub(super) fn attach_or_push_background_tool_start(&mut self, id: &str, command: &str) {
        let summary = format!("$ {command}");
        if let Some(run) = self
            .transcript
            .iter_mut()
            .rev()
            .find_map(|item| match item {
                TranscriptItem::Tool(run)
                    if run.id.is_none()
                        && run.name == "terminal.exec"
                        && run.summary == summary
                        && run.state == ToolRunState::Running =>
                {
                    Some(run)
                }
                _ => None,
            })
        {
            run.id = Some(id.to_string());
            self.touch_transcript();
        } else {
            self.push_tool_start_with_id(
                Some(id.to_string()),
                "terminal.exec".to_string(),
                summary,
            );
        }
    }

    pub(super) fn update_tool_result_by_id(&mut self, id: &str, state: ToolRunState, detail: &str) {
        let detail = compact_tool_detail(detail);
        if let Some(run) = self
            .transcript
            .iter_mut()
            .rev()
            .find_map(|item| match item {
                TranscriptItem::Tool(run) if run.id.as_deref() == Some(id) => Some(run),
                _ => None,
            })
        {
            queue_or_apply_tool_result(run, state, detail, state == ToolRunState::Failed);
            self.touch_transcript();
        }
        self.persist_session();
    }

    pub(super) fn close_selected_tool(&mut self) {
        let Some(index) = self.selected_tool else {
            self.status_line = "no tool selected".to_string();
            return;
        };

        if let Some(TranscriptItem::Reasoning(trace)) = self.transcript.get_mut(index) {
            trace.expanded = false;
            self.touch_transcript();
        }

        let mut changed = false;
        if let Some((start, end)) = self.tool_group_range_containing(index) {
            for item in &mut self.transcript[start..end] {
                if let TranscriptItem::Tool(run) = item {
                    run.expanded = false;
                    run.group_expanded = false;
                    changed = true;
                }
            }
        }

        if changed {
            self.touch_transcript();
        }
        self.selected_tool = None;
        self.status_line = "tool closed".to_string();
        self.persist_session();
    }

    pub(super) fn current_plan(&self) -> Option<&PlanView> {
        self.transcript.iter().rev().find_map(|item| match item {
            TranscriptItem::Plan(plan) => Some(plan),
            _ => None,
        })
    }

    pub(super) fn apply_plan_update_output(
        &mut self,
        output: &str,
    ) -> std::result::Result<(), String> {
        let mut plan = serde_json::from_str::<PlanView>(output)
            .map_err(|error| format!("could not parse plan.update output: {error}"))?;
        plan.expanded = false;

        if let Some(TranscriptItem::Plan(existing)) = self.transcript.last_mut() {
            *existing = plan;
        } else {
            self.transcript.push(TranscriptItem::Plan(plan));
        }

        self.touch_transcript();
        self.persist_session();
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn current_decision(&self) -> Option<&DecisionView> {
        self.transcript.iter().rev().find_map(|item| match item {
            TranscriptItem::Decision(decision) => Some(decision),
            _ => None,
        })
    }

    pub(super) fn pending_decision(&self) -> Option<&DecisionView> {
        self.transcript.iter().rev().find_map(|item| match item {
            TranscriptItem::Decision(decision) if !decision.answered => Some(decision),
            _ => None,
        })
    }

    pub(super) fn pending_decision_index(&self) -> Option<usize> {
        self.transcript.iter().rposition(
            |item| matches!(item, TranscriptItem::Decision(decision) if !decision.answered),
        )
    }

    pub(super) fn apply_decision_request_output(
        &mut self,
        output: &str,
    ) -> std::result::Result<(), String> {
        let mut decision = serde_json::from_str::<DecisionView>(output)
            .map_err(|error| format!("could not parse decision.request output: {error}"))?;
        decision.answered = false;
        decision.answer = None;
        decision.answers.clear();
        decision.expanded = false;
        self.decision_selection = 0;

        if let Some(TranscriptItem::Decision(existing)) = self.transcript.last_mut()
            && !existing.answered
        {
            *existing = decision;
        } else {
            self.transcript.push(TranscriptItem::Decision(decision));
        }

        self.touch_transcript();
        self.persist_session();
        Ok(())
    }

    pub(super) fn selected_decision_question_index(&self) -> usize {
        self.pending_decision()
            .map(|decision| {
                self.decision_selection
                    .min(decision.questions.len().saturating_sub(1))
            })
            .unwrap_or(0)
    }

    pub(super) fn handle_decision_key(&mut self, key: KeyEvent) -> bool {
        if self.pending_decision().is_none()
            || self.slash_suggestions_active()
            || self.mention_popup_visible()
        {
            return false;
        }

        match key.code {
            KeyCode::Down if self.input.is_empty() => {
                self.move_decision_selection(1);
                true
            }
            KeyCode::Up if self.input.is_empty() => {
                self.move_decision_selection(-1);
                true
            }
            KeyCode::Char('j') if self.input.is_empty() && self.selected_decision_is_choice() => {
                self.move_decision_selection(1);
                true
            }
            KeyCode::Char('k') if self.input.is_empty() && self.selected_decision_is_choice() => {
                self.move_decision_selection(-1);
                true
            }
            KeyCode::Char('h') | KeyCode::Left | KeyCode::BackTab
                if self.input.is_empty() && self.selected_decision_is_choice() =>
            {
                self.cycle_selected_decision_choice(-1);
                true
            }
            KeyCode::Char('l') | KeyCode::Right | KeyCode::Tab
                if self.input.is_empty() && self.selected_decision_is_choice() =>
            {
                self.cycle_selected_decision_choice(1);
                true
            }
            KeyCode::Char(ch)
                if self.input.is_empty()
                    && ch.is_ascii_digit()
                    && self.selected_decision_is_choice() =>
            {
                let Some(digit) = ch.to_digit(10) else {
                    return false;
                };
                if digit == 0 {
                    return false;
                }
                self.select_decision_option((digit as usize).saturating_sub(1));
                true
            }
            KeyCode::Char('j' | 'm') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.accept_decision_enter();
                true
            }
            KeyCode::Enter | KeyCode::Char('\n') | KeyCode::Char('\r') => {
                self.accept_decision_enter();
                true
            }
            _ => false,
        }
    }

    pub(super) fn selected_decision_is_choice(&self) -> bool {
        self.pending_decision()
            .and_then(|decision| {
                decision
                    .questions
                    .get(self.selected_decision_question_index())
            })
            .is_some_and(|question| question.kind == DecisionQuestionKind::Choice)
    }

    pub(super) fn move_decision_selection(&mut self, amount: isize) {
        let Some(decision) = self.pending_decision() else {
            return;
        };
        let count = decision.questions.len();
        if count == 0 {
            self.decision_selection = 0;
            return;
        }
        self.decision_selection = (self.selected_decision_question_index() as isize + amount)
            .rem_euclid(count as isize) as usize;
        self.status_line = format!(
            "decision question {}/{}",
            self.decision_selection + 1,
            count
        );
    }

    pub(super) fn cycle_selected_decision_choice(&mut self, amount: isize) {
        let Some((question_id, options, current)) = self.selected_decision_choice_state() else {
            self.status_line = "selected decision expects text".to_string();
            self.toast("Type an answer, then press enter", ToastKind::Info);
            return;
        };
        if options.is_empty() {
            self.status_line = "decision has no options".to_string();
            return;
        }
        let next = (current as isize + amount).rem_euclid(options.len() as isize) as usize;
        let value = options[next].clone();
        self.set_decision_answer(&question_id, value.clone());
        self.status_line = format!("decision option: {}", truncate(&value, 48));
    }

    pub(super) fn select_decision_option(&mut self, index: usize) {
        let Some((question_id, options, _)) = self.selected_decision_choice_state() else {
            self.status_line = "selected decision expects text".to_string();
            return;
        };
        let Some(value) = options.get(index).cloned() else {
            self.status_line = "option number unavailable".to_string();
            return;
        };
        self.set_decision_answer(&question_id, value.clone());
        self.status_line = format!("decision option: {}", truncate(&value, 48));
    }

    pub(super) fn selected_decision_choice_state(&self) -> Option<(String, Vec<String>, usize)> {
        let decision = self.pending_decision()?;
        let question = decision
            .questions
            .get(self.selected_decision_question_index())?;
        if question.kind != DecisionQuestionKind::Choice {
            return None;
        }
        let options = question.options.clone();
        if options.is_empty() {
            return Some((question.id.clone(), options, 0));
        }
        let selected = decision
            .answers
            .get(&question.id)
            .and_then(|answer| options.iter().position(|option| option == answer))
            .or_else(|| {
                question
                    .recommended
                    .as_ref()
                    .and_then(|recommended| options.iter().position(|option| option == recommended))
            })
            .unwrap_or(0);
        Some((question.id.clone(), options, selected))
    }

    pub(super) fn set_decision_answer(&mut self, question_id: &str, value: String) {
        let Some(index) = self.pending_decision_index() else {
            return;
        };
        if let Some(TranscriptItem::Decision(decision)) = self.transcript.get_mut(index) {
            decision
                .answers
                .insert(question_id.to_string(), value.chars().take(600).collect());
            self.touch_transcript();
            self.persist_session();
        }
    }

    pub(super) fn accept_decision_enter(&mut self) {
        if !self.input.trim().is_empty() {
            self.record_typed_decision_answer();
        } else {
            self.accept_empty_decision_enter();
        }

        if self.pending_decision().is_some_and(decision_ready) {
            self.submit_decision_answer();
        }
    }

    pub(super) fn record_typed_decision_answer(&mut self) {
        let Some((question_id, question_kind, options)) =
            self.pending_decision().and_then(|decision| {
                decision
                    .questions
                    .get(self.selected_decision_question_index())
                    .map(|question| (question.id.clone(), question.kind, question.options.clone()))
            })
        else {
            return;
        };
        let value = self.input.trim().to_string();
        if value.is_empty() {
            return;
        }

        if question_kind == DecisionQuestionKind::Choice {
            let Some(option) = match_choice_option(&options, &value) else {
                self.status_line = "choose an option with h/l or 1-8".to_string();
                self.toast(
                    "Choice question needs one of the listed options",
                    ToastKind::Warning,
                );
                return;
            };
            self.set_decision_answer(&question_id, option);
        } else {
            self.set_decision_answer(&question_id, value);
        }

        self.input.clear();
        self.input_cursor = 0;
        self.move_to_next_unanswered_decision();
    }

    pub(super) fn accept_empty_decision_enter(&mut self) {
        let Some((question_id, question_kind, options, recommended)) =
            self.pending_decision().and_then(|decision| {
                decision
                    .questions
                    .get(self.selected_decision_question_index())
                    .map(|question| {
                        (
                            question.id.clone(),
                            question.kind,
                            question.options.clone(),
                            question.recommended.clone(),
                        )
                    })
            })
        else {
            return;
        };

        if self
            .pending_decision()
            .is_some_and(|decision| decision.answers.contains_key(&question_id))
        {
            self.move_to_next_unanswered_decision();
            return;
        }

        if question_kind == DecisionQuestionKind::Choice {
            let choice = recommended
                .filter(|value| options.iter().any(|option| option == value))
                .or_else(|| options.first().cloned());
            if let Some(choice) = choice {
                self.set_decision_answer(&question_id, choice);
                self.move_to_next_unanswered_decision();
            }
        } else {
            self.status_line = "type an answer for this decision".to_string();
            self.toast("Type an answer, then press enter", ToastKind::Info);
        }
    }

    pub(super) fn move_to_next_unanswered_decision(&mut self) {
        let Some(decision) = self.pending_decision() else {
            return;
        };
        let count = decision.questions.len();
        if count == 0 {
            return;
        }
        for offset in 1..=count {
            let index = (self.selected_decision_question_index() + offset) % count;
            let question = &decision.questions[index];
            if question.required && !decision_question_answered(decision, question) {
                self.decision_selection = index;
                self.status_line = format!("decision question {}/{}", index + 1, count);
                return;
            }
        }
        self.status_line = "decision ready · press enter to send".to_string();
    }

    pub(super) fn submit_decision_answer(&mut self) {
        if self.is_working() || self.has_active_workflows() {
            self.status_line = "finish current work before answering decision".to_string();
            return;
        }

        let Some(index) = self.pending_decision_index() else {
            return;
        };
        let answer = {
            let Some(TranscriptItem::Decision(decision)) = self.transcript.get(index) else {
                return;
            };
            decision_answer_text(decision)
        };

        if let Some(TranscriptItem::Decision(decision)) = self.transcript.get_mut(index) {
            decision.answered = true;
            decision.answer = Some(answer.clone());
        }
        self.touch_transcript();
        self.transcript
            .push(TranscriptItem::Message(ChatMessage::user(answer.clone())));
        self.touch_transcript();
        self.persist_session();
        self.scroll_chat_to_bottom();
        self.toast("Decision answer sent", ToastKind::Success);
        self.status_line = "decision answer sent".to_string();
        self.start_model_turn(&answer);
    }

    pub(super) fn tool_group_indices(&self) -> Vec<usize> {
        let mut groups = Vec::new();
        let mut index = 0;
        while index < self.transcript.len() {
            match &self.transcript[index] {
                TranscriptItem::Message(_)
                | TranscriptItem::Workflow(_)
                | TranscriptItem::Plan(_)
                | TranscriptItem::Decision(_) => index += 1,
                TranscriptItem::Tool(_) | TranscriptItem::Reasoning(_) => {
                    let mut first_tool = None;
                    while index < self.transcript.len()
                        && matches!(
                            self.transcript[index],
                            TranscriptItem::Tool(_) | TranscriptItem::Reasoning(_)
                        )
                    {
                        if first_tool.is_none()
                            && matches!(self.transcript[index], TranscriptItem::Tool(_))
                        {
                            first_tool = Some(index);
                        }
                        index += 1;
                    }
                    if let Some(tool_index) = first_tool {
                        groups.push(tool_index);
                    }
                }
            }
        }
        groups
    }

    pub(super) fn tool_group_range_containing(&self, index: usize) -> Option<(usize, usize)> {
        if !matches!(self.transcript.get(index), Some(TranscriptItem::Tool(_))) {
            return None;
        }

        let mut start = index;
        while start > 0
            && matches!(
                self.transcript[start - 1],
                TranscriptItem::Tool(_) | TranscriptItem::Reasoning(_)
            )
        {
            start -= 1;
        }

        let mut end = index + 1;
        while end < self.transcript.len()
            && matches!(
                self.transcript[end],
                TranscriptItem::Tool(_) | TranscriptItem::Reasoning(_)
            )
        {
            end += 1;
        }

        Some((start, end))
    }
}
