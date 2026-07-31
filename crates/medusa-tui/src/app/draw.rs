use super::*;

impl App {
    pub(super) fn draw(&mut self, frame: &mut Frame<'_>) {
        let area = frame.area();
        frame.render_widget(
            Block::default().style(Style::default().bg(app_bg()).fg(text())),
            area,
        );

        let shell_area = area.inner(Margin {
            horizontal: 1,
            vertical: 0,
        });
        let input_height = self.input_height(area.height);
        let plan_height = self.plan_strip_height(area.height);
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Min(5),
                Constraint::Length(plan_height),
                Constraint::Length(input_height),
                Constraint::Length(1),
            ])
            .split(shell_area);

        self.draw_header(frame, sections[0]);
        self.draw_workspace(frame, sections[1]);
        self.draw_plan_strip(frame, sections[2]);
        self.draw_input(frame, sections[3]);
        self.draw_status(frame, sections[4]);
        if self.active_modal.is_none() {
            self.draw_slash_suggestions(frame, shell_area);
            self.draw_mention_suggestions(frame, shell_area);
        }
        self.draw_modal(frame, shell_area);
        self.draw_approval_prompt(frame, shell_area);
    }

    pub(super) fn focus(&self) -> UiFocus {
        if self.active_modal.is_some() {
            UiFocus::Modal
        } else if self.selected_tool.is_some() {
            UiFocus::Activity
        } else if self.chat_scroll > 0 {
            UiFocus::Transcript
        } else {
            UiFocus::Composer
        }
    }

    pub(super) fn draw_workspace(&mut self, frame: &mut Frame<'_>, area: Rect) {
        self.draw_messages(frame, area);
    }

    pub(super) fn draw_header(&self, frame: &mut Frame<'_>, area: Rect) {
        if area.height == 0 {
            return;
        }

        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Length(1)])
            .split(area);
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(40), Constraint::Length(52)])
            .split(sections[0]);
        let (state_label, state_style) = self.header_state();
        let session = self
            .session
            .as_ref()
            .map(|session| compact_session_id(&session.current_id()))
            .unwrap_or_else(|| "no-session".to_string());
        let left = Paragraph::new(Line::from(vec![
            Span::styled(
                " MEDUSA ",
                Style::default()
                    .fg(palette().selected_fg)
                    .bg(accent_color())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("  ", muted()),
            Span::styled("● ", state_style),
            Span::styled(state_label, state_style),
            Span::styled("  workspace ", muted()),
            Span::styled(self.cwd_display.clone(), value_style()),
        ]))
        .style(Style::default().bg(surface()).fg(text()));
        // Left side: state + workspace
        frame.render_widget(left, columns[0]);

        // Right side: model · effort / perm / git / session info
        {
            let mut right_spans = vec![];
            right_spans.push(Span::styled(
                self.model.model_name().to_string(),
                tool_label_style().add_modifier(Modifier::BOLD),
            ));
            // Reasoning effort rides alongside the model; "none" means disabled.
            let effort = self.model.reasoning_effort();
            if !effort.is_empty() {
                right_spans.push(Span::styled(format!(" {effort}"), accent()));
            }
            right_spans.push(Span::styled("  ", muted()));
            right_spans.push(Span::styled("perm ", muted()));
            right_spans.push(Span::styled(self.permission_mode.name(), value_style()));
            right_spans.push(Span::styled("  ", muted()));
            right_spans.push(Span::styled(
                if self.inside_git_repo {
                    "git"
                } else {
                    "no-git"
                },
                if self.inside_git_repo {
                    success_style()
                } else {
                    muted()
                },
            ));
            right_spans.push(Span::styled("  ", muted()));
            right_spans.push(Span::styled(session, muted()));
            let right = Paragraph::new(Line::from(right_spans))
                .alignment(Alignment::Right)
                .style(Style::default().bg(surface()).fg(text()));
            frame.render_widget(right, columns[1]);
        }

        let rule = Paragraph::new(Line::from(Span::styled(
            "─".repeat(sections[1].width as usize),
            separator_style(),
        )))
        .style(Style::default().bg(surface()));
        frame.render_widget(rule, sections[1]);
    }

    pub(super) fn header_state(&self) -> (&'static str, Style) {
        if self.is_working() {
            ("working", tool_label_style())
        } else if self.has_active_workflows() {
            ("workflow", prompt_style())
        } else if self.has_running_tool_rows() {
            ("tools", tool_label_style())
        } else if self.permission_mode == PermissionMode::Readonly {
            ("readonly", prompt_style())
        } else if self.permission_mode == PermissionMode::Guarded {
            ("guarded", prompt_style())
        } else {
            ("ready", success_style())
        }
    }

    pub(super) fn draw_messages(&mut self, frame: &mut Frame<'_>, area: Rect) {
        self.last_chat_viewport = Some(area);
        let rows = self.visible_transcript_rows_cached();
        let metrics = chat_viewport_metrics(&rows, area, self.chat_scroll);
        self.chat_scroll = metrics.scroll;
        let window = transcript_viewport_window(
            &rows,
            metrics.text_area.width,
            metrics.top_offset,
            metrics.text_area.height as usize,
        );
        let chat_lines = transcript_lines_from_rows(&window.rows);

        let chat = Paragraph::new(chat_lines)
            .style(Style::default().bg(surface()))
            .wrap(Wrap { trim: false })
            .scroll((paragraph_scroll_offset(window.scroll_offset), 0));
        frame.render_widget(chat, metrics.text_area);
        self.render_transcript_images(frame, metrics.text_area, &rows, metrics.top_offset);
        self.draw_transcript_scroll_thumb(frame, area, metrics);
        self.last_transcript_rows = rows;
    }

    pub(super) fn draw_transcript_scroll_thumb(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        metrics: ChatViewportMetrics,
    ) {
        if !metrics.has_scrollbar || metrics.scroll == 0 || area.width == 0 || area.height == 0 {
            return;
        }

        let mut state = ScrollbarState::new(metrics.max_scroll.max(1)).position(metrics.top_offset);
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .track_symbol(Some(" "))
            .thumb_symbol("▌")
            .track_style(Style::default().bg(surface()))
            .thumb_style(accent().bg(surface()));
        frame.render_stateful_widget(scrollbar, area, &mut state);
    }

    pub(super) fn visible_transcript_rows(&self) -> Vec<TranscriptRow> {
        visible_transcript_rows(
            &self.transcript,
            self.streaming_message,
            self.selected_tool,
            RenderContext {
                animation_tick: self.animation_tick,
                decision_selection: self.decision_selection,
            },
        )
    }

    pub(super) fn visible_transcript_rows_cached(&mut self) -> Arc<Vec<TranscriptRow>> {
        let animation_tick = if self.has_running_tool_rows() || self.has_running_workflow_rows() {
            Some(self.animation_tick)
        } else {
            None
        };
        if let Some(cache) = &self.transcript_rows_cache
            && cache.version == self.transcript_version
            && cache.theme == self.theme
            && cache.streaming_message == self.streaming_message
            && cache.selected_tool == self.selected_tool
            && cache.animation_tick == animation_tick
            && cache.decision_selection == self.decision_selection
        {
            return Arc::clone(&cache.rows);
        }

        let rows = Arc::new(self.visible_transcript_rows());
        self.transcript_rows_cache = Some(TranscriptRowsCache {
            version: self.transcript_version,
            theme: self.theme,
            streaming_message: self.streaming_message,
            selected_tool: self.selected_tool,
            animation_tick,
            decision_selection: self.decision_selection,
            rows: Arc::clone(&rows),
        });
        rows
    }

    pub(super) fn render_transcript_images(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        rows: &[TranscriptRow],
        top_offset: usize,
    ) {
        if area.width < 8 || area.height == 0 {
            return;
        }

        for placement in transcript_image_placements(rows, area, top_offset) {
            self.image_renderer.render(
                frame,
                &placement.attachment,
                area,
                placement.width,
                placement.height,
                placement.x_offset,
                placement.y_offset,
            );
        }
    }

    pub(super) fn draw_input(&self, frame: &mut Frame<'_>, area: Rect) {
        let mut display = Vec::new();
        if !self.pending_attachments.is_empty() {
            display.push(attachment_strip_line(&self.pending_attachments));
            display.extend(composer_attachment_preview_lines(
                &self.pending_attachments,
                &self.attachment_previews,
                area.width,
            ));
        }
        display.extend(input_display_lines(
            &self.input,
            self.input_cursor,
            area.height.saturating_sub(2) as usize,
        ));
        display = vertically_center_input_lines(display, area.height.saturating_sub(2));
        let border_style = if self.model_events.is_some() {
            muted()
        } else {
            Style::default().fg(accent_color())
        };

        let input = Paragraph::new(display)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .title(self.input_title_content())
                    .border_style(border_style)
                    .style(Style::default().bg(surface()).fg(text()))
                    .padding(Padding::new(1, 1, 0, 0)),
            )
            .wrap(Wrap { trim: false });

        frame.render_widget(input, area);
    }

    /// The plan shown in the strip above the composer: the latest plan while
    /// it still has unfinished work. Completed plans leave the screen.
    pub(super) fn plan_strip(&self) -> Option<&PlanView> {
        let plan = self.current_plan()?;
        if plan.items.is_empty()
            || plan
                .items
                .iter()
                .all(|item| item.status == PlanItemStatus::Done)
        {
            return None;
        }
        Some(plan)
    }

    pub(super) fn plan_strip_height(&self, terminal_height: u16) -> u16 {
        // On short terminals the transcript and composer win.
        if terminal_height < 20 {
            return 0;
        }
        match self.plan_strip() {
            Some(plan) => (plan_strip_lines(plan).len() as u16).min(9),
            None => 0,
        }
    }

    pub(super) fn draw_plan_strip(&self, frame: &mut Frame<'_>, area: Rect) {
        if area.height == 0 {
            return;
        }
        let Some(plan) = self.plan_strip() else {
            return;
        };
        let mut lines = plan_strip_lines(plan);
        lines.truncate(area.height as usize);
        frame.render_widget(
            Paragraph::new(lines).style(Style::default().bg(app_bg()).fg(text())),
            area,
        );
    }

    pub(super) fn input_height(&self, terminal_height: u16) -> u16 {
        let attachment_lines = if self.pending_attachments.is_empty() {
            0
        } else {
            1 + COMPOSER_IMAGE_PREVIEW_HEIGHT
        };
        let text_lines = self.input.lines().count().max(1) as u16;
        let desired = (text_lines + attachment_lines + 2).clamp(3, 8);
        let max = terminal_height.saturating_sub(4).max(3);

        desired.min(max)
    }

    pub(super) fn draw_status(&self, frame: &mut Frame<'_>, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Min(16),
                Constraint::Min(0),
                Constraint::Length(10),
                Constraint::Length(42),
            ])
            .split(area);
        let status = Paragraph::new(self.status_line_content())
            .alignment(Alignment::Left)
            .style(Style::default().bg(surface()));

        frame.render_widget(status, chunks[0]);
        self.draw_footer_hints(frame, chunks[1]);
        self.draw_context_gauge(frame, chunks[2]);
        self.draw_footer_telemetry(frame, chunks[3]);
    }

    pub(super) fn context_usage_chars(&self) -> usize {
        transcript_char_usage(&self.transcript).total()
    }

    /// Snapshot for the /context modal: estimated tokens per category, the
    /// budget, and the compaction state. Uses the same ~4 chars/token
    /// estimate as the footer gauge and the core context engine.
    pub(super) fn build_context_report(&self) -> ContextReport {
        let chars = transcript_char_usage(&self.transcript);
        // The header system messages conversation_history() prepends before
        // the transcript-derived messages (permission context, rolling
        // session state, optional plan directive).
        let header_len = 2 + usize::from(self.plan_mode);
        let system_tokens = self
            .conversation_history()
            .iter()
            .take(header_len)
            .map(medusa_core::context::message_tokens)
            .sum();
        let summary = self.context_engine.summary();

        ContextReport {
            instructions_tokens: medusa_core::context::baseline_instructions_tokens(
                self.tools.workspace(),
            ),
            system_tokens,
            message_tokens: chars.messages.div_ceil(4),
            tool_tokens: chars.tool_outputs.div_ceil(4),
            reasoning_tokens: chars.reasoning.div_ceil(4),
            plan_tokens: chars.plans.div_ceil(4),
            budget: medusa_core::context::context_max_tokens(),
            summary_covers: summary.as_ref().map(|summary| summary.covers),
            summary_tokens: summary
                .map(|summary| medusa_core::context::estimate_tokens(&summary.text))
                .unwrap_or(0),
        }
    }

    pub(super) fn draw_context_gauge(&self, frame: &mut Frame<'_>, area: Rect) {
        if area.width < 4 {
            return;
        }
        // ~4 chars per token, matching medusa_core::context::estimate_tokens.
        let used = self.context_usage_chars().div_ceil(4);
        let max = medusa_core::context::context_max_tokens().max(1);
        let ratio = (used as f64 / max as f64).clamp(0.0, 1.0);
        let color = if ratio < 0.5 {
            palette().success
        } else if ratio < 0.8 {
            accent_color()
        } else {
            palette().error
        };
        let gauge = LineGauge::default()
            .filled_style(Style::default().fg(color).bg(surface()))
            .unfilled_style(Style::default().fg(palette().separator).bg(surface()))
            .ratio(ratio)
            .label(Span::styled(
                if area.width >= 8 {
                    format!("{:>3.0}%", ratio * 100.0)
                } else {
                    String::new()
                },
                muted(),
            ))
            .style(Style::default().bg(surface()));
        frame.render_widget(gauge, area);
    }

    pub(super) fn draw_footer_telemetry(&self, frame: &mut Frame<'_>, area: Rect) {
        let running = self
            .background_jobs
            .values()
            .filter(|job| job.state == ToolRunState::Running)
            .count();
        let activity = self
            .turn_started_at
            .map(|t| format!("turn {}s", t.elapsed().as_secs()))
            .unwrap_or_else(|| self.scroll_footer_label());
        let tool_count = self
            .transcript
            .iter()
            .filter(|item| matches!(item, TranscriptItem::Tool(_)))
            .count();
        let text = format!(
            "tools {tool_count} · jobs {running} · {} · {activity}",
            format_token_count(self.session_usage.total())
        );
        let widget = Paragraph::new(Line::from(Span::styled(text, muted())))
            .alignment(Alignment::Left)
            .style(Style::default().bg(surface()));
        frame.render_widget(widget, area);
    }

    pub(super) fn scroll_footer_label(&self) -> String {
        let Some(metrics) = self.current_chat_viewport_metrics() else {
            return "idle".to_string();
        };
        if metrics.max_scroll == 0 {
            return "idle".to_string();
        }
        if metrics.scroll == 0 {
            return "bottom".to_string();
        }
        format!("scroll {}%", scroll_progress_percent(&metrics))
    }

    pub(super) fn status_line_content(&self) -> Line<'static> {
        let Some(toast) = &self.toast else {
            return Line::from(Span::styled(self.status_line.clone(), muted()));
        };

        Line::from(vec![
            Span::styled(
                toast_label(toast.kind),
                toast_style(toast.kind).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" · ", muted()),
            Span::styled(truncate(&toast.message, 96), value_style()),
        ])
    }

    pub(super) fn draw_footer_hints(&self, frame: &mut Frame<'_>, area: Rect) {
        let hints = match self.focus() {
            UiFocus::Activity => "j/k · enter · x · esc",
            UiFocus::Transcript => "ctrl+end · pgup/dn",
            UiFocus::Modal => match self.active_modal {
                Some(Modal::Settings) => "↑/↓ · enter · esc",
                Some(Modal::Themes) => "↑/↓ · enter · esc",
                Some(Modal::Rewind) => "↑/↓ · enter · esc",
                Some(Modal::EditMessage) => "↑/↓ · enter · esc",
                Some(Modal::Models) => "↑/↓ choose · ←/→ pane · enter · esc",
                Some(Modal::Permissions) => "↑/↓ · enter · esc",
                Some(Modal::ImagePreview) => "j/k · +/- · d detach · o/y · esc",
                _ => "esc",
            },
            UiFocus::Composer if self.pending_decision().is_some() => {
                "j/k question · h/l option · 1-8 choose · enter send"
            }
            UiFocus::Composer => "enter · ctrl+p · ctrl+i/o/d",
        };
        let footer = Paragraph::new(Line::from(Span::styled(hints, muted())))
            .alignment(Alignment::Left)
            .style(Style::default().bg(surface()));
        frame.render_widget(footer, area);
    }

    pub(super) fn input_title_content(&self) -> Line<'static> {
        if self.is_working() {
            return Line::from(light_sweep_spans(
                " ━━━━━━━ ",
                self.animation_tick,
                |style| style.bg(surface()),
            ));
        }

        if let Some(decision) = self.pending_decision() {
            let question = decision
                .questions
                .get(self.selected_decision_question_index())
                .map(|question| question.prompt.as_str())
                .unwrap_or(decision.title.as_str());
            return Line::from(vec![
                Span::styled(" Decision ", prompt_style().add_modifier(Modifier::BOLD)),
                Span::styled(
                    format!(" {} ", truncate(question, 48)),
                    accent().add_modifier(Modifier::BOLD),
                ),
            ]);
        }

        let mut spans = vec![
            Span::styled(" Message ", muted().add_modifier(Modifier::BOLD)),
            Span::styled(
                format!(" {} ", self.model.model_name()),
                accent().add_modifier(Modifier::BOLD),
            ),
        ];
        if self.plan_mode {
            spans.push(Span::styled(
                " plan ",
                Style::default()
                    .fg(palette().selected_fg)
                    .bg(palette().prompt)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        Line::from(spans)
    }
}
