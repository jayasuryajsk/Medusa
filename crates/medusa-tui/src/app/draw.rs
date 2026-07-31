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

    /// Always-on-top approval prompt for the front of the queue. Drawn last
    /// so it overlays modals and streaming output alike.
    pub(super) fn draw_approval_prompt(&self, frame: &mut Frame<'_>, area: Rect) {
        let Some(pending) = self.approval_queue.front() else {
            return;
        };
        let request = &pending.request;

        // Wrap width chosen to match the popup body so the full command shows
        // and a destructive tail can never hide past a truncation point.
        let wrap_width = area.width.saturating_sub(12).clamp(28, 72) as usize;
        let mut body: Vec<Line<'static>> = Vec::new();
        if let Some(command) = request.command.as_deref() {
            for (index, chunk) in wrap_str(command.trim(), wrap_width).into_iter().enumerate() {
                let prefix = if index == 0 { "  $ " } else { "    " };
                body.push(Line::from(vec![
                    Span::styled(prefix, prompt_style()),
                    Span::styled(chunk, value_style().add_modifier(Modifier::BOLD)),
                ]));
            }
            if request.background {
                body.push(Line::from(Span::styled(
                    "    runs as a background job",
                    muted(),
                )));
            }
            if request.sandbox_escalation {
                body.push(Line::from(Span::styled(
                    "    escapes the sandbox: writes outside the workspace and network allowed",
                    error_style(),
                )));
            }
        }
        for path in request.paths.iter().take(6) {
            body.push(Line::from(vec![
                Span::styled("  → ", separator_style()),
                Span::styled(truncate(path, wrap_width).to_string(), value_style()),
            ]));
        }
        if request.paths.len() > 6 {
            body.push(Line::from(Span::styled(
                format!("    … +{} more files", request.paths.len() - 6),
                muted(),
            )));
        }
        body.push(Line::from(""));
        let mut keys = vec![
            Span::styled("  y", success_style().add_modifier(Modifier::BOLD)),
            Span::styled(" allow once   ", muted()),
        ];
        // Escalations are one-shot by design: no always-allow.
        if !request.sandbox_escalation {
            keys.push(Span::styled(
                "a",
                prompt_style().add_modifier(Modifier::BOLD),
            ));
            keys.push(Span::styled(" always allow   ", muted()));
        }
        keys.extend([
            Span::styled("n", error_style().add_modifier(Modifier::BOLD)),
            Span::styled("/", muted()),
            Span::styled("esc", error_style().add_modifier(Modifier::BOLD)),
            Span::styled(" deny", muted()),
        ]);
        body.push(Line::from(keys));

        let height = (body.len() as u16)
            .saturating_add(2)
            .min(area.height.saturating_sub(2).max(4));
        let width = area
            .width
            .saturating_sub(8)
            .min(78)
            .min(area.width.saturating_sub(2))
            .max(area.width.min(30));
        let x = area.x + (area.width.saturating_sub(width)) / 2;
        let y = area
            .y
            .saturating_add(area.height.saturating_sub(height + 6));
        let popup = Rect {
            x,
            y,
            width,
            height,
        };

        frame.render_widget(Clear, popup);
        let queued = self.approval_queue.len();
        let heading = if request.sandbox_escalation {
            "Run unsandboxed?"
        } else {
            "Approval required"
        };
        let title = if queued > 1 {
            format!(" {heading} · {} (1/{queued}) ", request.tool.label())
        } else {
            format!(" {heading} · {} ", request.tool.label())
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(palette().prompt))
            .style(Style::default().bg(surface()).fg(text()))
            .title(title);
        let inner = popup.inner(Margin {
            horizontal: 1,
            vertical: 1,
        });
        frame.render_widget(block, popup);
        frame.render_widget(
            Paragraph::new(body).style(Style::default().bg(surface()).fg(text())),
            inner,
        );
    }

    pub(super) fn draw_slash_suggestions(&self, frame: &mut Frame<'_>, shell_area: Rect) {
        let matches = self.slash_matches();
        if matches.is_empty() {
            return;
        }

        let area = command_palette_rect(shell_area, matches.len());
        frame.render_widget(Clear, area);
        let selected = self.slash_selection.min(matches.len().saturating_sub(1));
        let query = self.input.trim_start_matches('/');
        let inner = area.inner(Margin {
            horizontal: 2,
            vertical: 1,
        });
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .split(inner);
        let body = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(42), Constraint::Min(26)])
            .split(sections[1]);

        let visible_rows = body[0].height as usize;
        let offset = selected
            .saturating_add(1)
            .saturating_sub(visible_rows.max(1));
        let end = offset.saturating_add(visible_rows).min(matches.len());
        let items = matches[offset..end]
            .iter()
            .map(|(command, positions)| {
                let mut spans = highlighted_command_name_spans(command.name, positions, 11);
                spans.push(Span::styled(format!("{:<9}", command.category), muted()));
                spans.push(Span::styled(command.args, value_style()));
                ListItem::new(Line::from(spans))
            })
            .collect::<Vec<_>>();
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(accent_color()))
            .style(Style::default().bg(surface()).fg(text()))
            .title(" Command Palette ");
        frame.render_widget(block, area);

        let header = Paragraph::new(vec![
            Line::from(vec![
                Span::styled("MEDUSA", accent().add_modifier(Modifier::BOLD)),
                Span::styled(
                    " command surface",
                    value_style().add_modifier(Modifier::BOLD),
                ),
                Span::styled("  ", muted()),
                Span::styled(format!("{} matches", matches.len()), muted()),
            ]),
            Line::from(vec![
                Span::styled("query ", muted()),
                Span::styled(
                    if query.is_empty() {
                        "/".to_string()
                    } else {
                        format!("/{query}")
                    },
                    prompt_style(),
                ),
            ]),
        ])
        .style(Style::default().bg(surface()).fg(text()));
        frame.render_widget(header, sections[0]);

        let mut state = ListState::default().with_selected(selected.checked_sub(offset));
        let list = List::new(items)
            .style(Style::default().bg(surface()).fg(text()))
            .highlight_style(command_selected_style())
            .highlight_symbol("▌ ");

        frame.render_stateful_widget(list, body[0], &mut state);

        let divider_area = Rect::new(
            body[0].x.saturating_add(body[0].width),
            body[0].y,
            1,
            body[0].height,
        );
        let divider = Paragraph::new(
            (0..divider_area.height)
                .map(|_| Line::from(Span::styled("│", separator_style())))
                .collect::<Vec<_>>(),
        )
        .style(Style::default().bg(surface()));
        frame.render_widget(divider, divider_area);

        let command = matches[selected].0;
        let detail = Paragraph::new(command_palette_detail_lines(command))
            .style(Style::default().bg(surface()).fg(text()))
            .wrap(Wrap { trim: true });
        frame.render_widget(
            detail,
            body[1].inner(Margin {
                horizontal: 2,
                vertical: 0,
            }),
        );

        let footer = Paragraph::new(Line::from(vec![
            Span::styled("↑/↓ tab", prompt_style()),
            Span::styled(" select  ", muted()),
            Span::styled("pg", prompt_style()),
            Span::styled(" jump  ", muted()),
            Span::styled("enter", prompt_style()),
            Span::styled(" run  ", muted()),
            Span::styled("esc", prompt_style()),
            Span::styled(" close", muted()),
        ]))
        .alignment(Alignment::Right)
        .style(Style::default().bg(surface()));
        frame.render_widget(footer, sections[2]);
    }

    /// @file mention popup: same centered palette surface as the slash
    /// suggestions, but a single fuzzy-filtered list of workspace paths.
    pub(super) fn draw_mention_suggestions(&self, frame: &mut Frame<'_>, shell_area: Rect) {
        if !self.mention_active() {
            return;
        }
        let matches = self.mention_matches();
        if matches.is_empty() {
            return;
        }

        let area = command_palette_rect(shell_area, matches.len());
        frame.render_widget(Clear, area);
        let selected = self.mention_selection.min(matches.len().saturating_sub(1));
        let query = self
            .active_mention_token()
            .map(|(_, _, query)| query)
            .unwrap_or_default();

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(accent_color()))
            .style(Style::default().bg(surface()).fg(text()))
            .title(" Files ");
        frame.render_widget(block, area);

        let inner = area.inner(Margin {
            horizontal: 2,
            vertical: 1,
        });
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .split(inner);

        let header = Paragraph::new(Line::from(vec![
            Span::styled("@", prompt_style()),
            Span::styled(query, prompt_style()),
            Span::styled("  ", muted()),
            Span::styled(format!("{} files", matches.len()), muted()),
        ]))
        .style(Style::default().bg(surface()).fg(text()));
        frame.render_widget(header, sections[0]);

        let visible_rows = sections[1].height as usize;
        let offset = selected
            .saturating_add(1)
            .saturating_sub(visible_rows.max(1));
        let end = offset.saturating_add(visible_rows).min(matches.len());
        let items = matches[offset..end]
            .iter()
            .map(|(path, positions)| ListItem::new(Line::from(mention_path_spans(path, positions))))
            .collect::<Vec<_>>();
        let mut state = ListState::default().with_selected(selected.checked_sub(offset));
        let list = List::new(items)
            .style(Style::default().bg(surface()).fg(text()))
            .highlight_style(command_selected_style())
            .highlight_symbol("▌ ");
        frame.render_stateful_widget(list, sections[1], &mut state);

        let footer = Paragraph::new(Line::from(vec![
            Span::styled("↑/↓", prompt_style()),
            Span::styled(" select  ", muted()),
            Span::styled("enter/tab", prompt_style()),
            Span::styled(" insert path  ", muted()),
            Span::styled("esc", prompt_style()),
            Span::styled(" close", muted()),
        ]))
        .alignment(Alignment::Right)
        .style(Style::default().bg(surface()));
        frame.render_widget(footer, sections[2]);
    }

    pub(super) fn draw_modal(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let Some(modal) = self.active_modal else {
            return;
        };

        let popup_width = match modal {
            Modal::ImagePreview => area.width.saturating_sub(4).min(128),
            Modal::Settings | Modal::Themes => area.width.saturating_sub(8).min(94),
            Modal::Models => area.width.saturating_sub(8).min(104),
            Modal::Permissions => area.width.saturating_sub(8).min(88),
            _ => area.width.saturating_sub(8).min(78),
        };
        let popup_height = area.height.saturating_sub(4).min(match modal {
            Modal::Commands => 18,
            Modal::Settings => 18,
            Modal::Help => 17,
            Modal::ImagePreview => 36,
            Modal::Workflows => 18,
            Modal::Jobs => 16,
            Modal::Sessions => 14,
            Modal::SessionTree => 18,
            Modal::Models => 18,
            Modal::Reasoning | Modal::Permissions => 16,
            Modal::Themes => 18,
            Modal::Rewind => 16,
            Modal::EditMessage => 18,
            Modal::Mcp => 20,
            Modal::Agents => 18,
            Modal::Cost => 14,
            Modal::Context => 18,
        });
        let popup = centered_rect(area, popup_width, popup_height);
        frame.render_widget(Clear, popup);

        match modal {
            Modal::Commands => self.draw_commands_modal(frame, popup),
            Modal::Settings => self.draw_settings_modal(frame, popup),
            Modal::Help => self.draw_help_modal(frame, popup),
            Modal::ImagePreview => self.draw_image_preview_modal(frame, popup),
            Modal::Workflows => self.draw_workflows_modal(frame, popup),
            Modal::Jobs => self.draw_jobs_modal(frame, popup),
            Modal::Sessions => self.draw_sessions_modal(frame, popup),
            Modal::SessionTree => self.draw_session_tree_modal(frame, popup),
            Modal::Models => self.draw_models_modal(frame, popup),
            Modal::Reasoning => self.draw_reasoning_modal(frame, popup),
            Modal::Permissions => self.draw_permissions_modal(frame, popup),
            Modal::Themes => self.draw_themes_modal(frame, popup),
            Modal::Rewind => self.draw_rewind_modal(frame, popup),
            Modal::EditMessage => self.draw_edit_message_modal(frame, popup),
            Modal::Mcp => self.draw_mcp_modal(frame, popup),
            Modal::Agents => self.draw_agents_modal(frame, popup),
            Modal::Cost => self.draw_cost_modal(frame, popup),
            Modal::Context => self.draw_context_modal(frame, popup),
        }
    }

    /// `/agents`: named agents from .medusa/agents captured when the command
    /// ran. Esc/Enter closes (generic modal keys).
    pub(super) fn draw_agents_modal(&self, frame: &mut Frame<'_>, area: Rect) {
        let mut lines: Vec<Line<'static>> = Vec::new();
        if self.agent_registry.is_empty() {
            lines.push(Line::from(Span::styled(
                "No named agents found in .medusa/agents.",
                muted(),
            )));
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "Define one agent per .md file in .medusa/agents:",
                value_style(),
            )));
            lines.push(Line::from(Span::styled(
                "  header lines name:, description:, tools: read|shell|edit|verify,",
                muted(),
            )));
            lines.push(Line::from(Span::styled(
                "  then a blank line, then the body used as the agent's system prompt.",
                muted(),
            )));
        }
        for agent in self.agent_registry.agents() {
            lines.push(Line::from(vec![
                Span::styled(
                    agent.name.clone(),
                    value_style().add_modifier(Modifier::BOLD),
                ),
                Span::styled("  · ", muted()),
                Span::styled(agent.tool_policy.label(), prompt_style()),
            ]));
            if !agent.description.is_empty() {
                lines.push(Line::from(Span::styled(
                    format!("  {}", truncate(&agent.description, 70)),
                    value_style(),
                )));
            }
            lines.push(Line::from(Span::styled(
                format!("  {}", truncate(&agent.path.display().to_string(), 70)),
                muted(),
            )));
            lines.push(Line::from(""));
        }
        for warning in self.agent_registry.warnings() {
            lines.push(Line::from(Span::styled(
                truncate(warning, 74),
                error_style(),
            )));
        }

        let paragraph = Paragraph::new(lines)
            .block(modal_block(" Named agents "))
            .style(Style::default().bg(surface()).fg(text()))
            .wrap(Wrap { trim: false });
        frame.render_widget(paragraph, area);
    }

    /// `/cost`: backend-reported token usage for the session and the most
    /// recent (or streaming) turn. Esc/Enter closes (generic modal keys).
    pub(super) fn draw_cost_modal(&self, frame: &mut Frame<'_>, area: Rect) {
        let usage_line = |usage: TokenUsage| {
            Line::from(Span::styled(
                format!(
                    "  input {} · output {} · cached {}",
                    format_token_count(usage.input),
                    format_token_count(usage.output),
                    format_token_count(usage.cached)
                ),
                value_style(),
            ))
        };
        let request_count =
            |requests: usize| format!("{requests} request{}", if requests == 1 { "" } else { "s" });
        let (turn_label, turn_usage, turn_requests) = if self.is_working() {
            ("current turn", self.turn_usage, self.turn_requests)
        } else {
            ("last turn", self.last_turn_usage, self.last_turn_requests)
        };

        let mut lines = vec![
            Line::from(vec![
                Span::styled("session", value_style().add_modifier(Modifier::BOLD)),
                Span::styled(
                    format!(
                        "  · {} · {} total",
                        request_count(self.session_requests),
                        format_token_count(self.session_usage.total())
                    ),
                    muted(),
                ),
            ]),
            usage_line(self.session_usage),
            Line::from(""),
            Line::from(vec![
                Span::styled(turn_label, value_style().add_modifier(Modifier::BOLD)),
                Span::styled(
                    format!(
                        "  · {} · {} total",
                        request_count(turn_requests),
                        format_token_count(turn_usage.total())
                    ),
                    muted(),
                ),
            ]),
            usage_line(turn_usage),
            Line::from(""),
        ];
        if self.session_requests == 0 {
            lines.push(Line::from(Span::styled(
                "No usage reported yet — counts appear after the first model turn.",
                muted(),
            )));
            lines.push(Line::from(""));
        }
        lines.push(Line::from(Span::styled(
            "Counts are backend-reported tokens. What they cost depends on your plan and provider pricing; Medusa does not estimate dollar amounts.",
            muted(),
        )));

        let paragraph = Paragraph::new(lines)
            .block(modal_block(" Token usage "))
            .style(Style::default().bg(surface()).fg(text()))
            .wrap(Wrap { trim: false });
        frame.render_widget(paragraph, area);
    }

    /// `/context`: the estimated context picture captured when the command
    /// ran — per-category token estimates, the budget, and compaction state.
    /// Esc/Enter closes (generic modal keys).
    pub(super) fn draw_context_modal(&self, frame: &mut Frame<'_>, area: Rect) {
        let Some(report) = self.context_report else {
            return;
        };

        let category_line = |label: &str, tokens: usize| {
            Line::from(vec![
                Span::styled(format!("  {label:<22}"), value_style()),
                Span::styled(format_token_count(tokens as u64), prompt_style()),
            ])
        };
        let percent = report.percent_used();
        let percent_style = if percent < 50 {
            success_style()
        } else if percent < 80 {
            prompt_style()
        } else {
            error_style()
        };

        let mut lines = vec![
            Line::from(vec![
                Span::styled(
                    "estimated usage",
                    value_style().add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(
                        "  · {} of {} budget · ",
                        format_token_count(report.total_tokens() as u64),
                        format_token_count(report.budget as u64)
                    ),
                    muted(),
                ),
                Span::styled(format!("{percent}%"), percent_style),
            ]),
            Line::from(""),
            category_line("system prompt (est)", report.instructions_tokens),
            category_line("session headers", report.system_tokens),
            category_line("messages", report.message_tokens),
            category_line("tool outputs", report.tool_tokens),
            category_line("reasoning", report.reasoning_tokens),
            category_line("plans & decisions", report.plan_tokens),
            Line::from(""),
        ];
        match report.summary_covers {
            Some(covers) => lines.push(Line::from(vec![
                Span::styled("compaction  ", value_style().add_modifier(Modifier::BOLD)),
                Span::styled(
                    format!(
                        "summary active · covers {covers} older messages · ~{}",
                        format_token_count(report.summary_tokens as u64)
                    ),
                    success_style(),
                ),
            ])),
            None => lines.push(Line::from(vec![
                Span::styled("compaction  ", value_style().add_modifier(Modifier::BOLD)),
                Span::styled("none — run /compact to fold older history early", muted()),
            ])),
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Estimates use ~4 characters per token; the backend's own count is what /cost reports.",
            muted(),
        )));

        let paragraph = Paragraph::new(lines)
            .block(modal_block(" Context "))
            .style(Style::default().bg(surface()).fg(text()))
            .wrap(Wrap { trim: false });
        frame.render_widget(paragraph, area);
    }

    /// `/mcp`: the snapshot captured when the command ran — servers, states,
    /// tools, and the config hint. Esc/Enter closes (generic modal keys).
    pub(super) fn draw_mcp_modal(&self, frame: &mut Frame<'_>, area: Rect) {
        let mut lines: Vec<Line<'static>> = Vec::new();
        if self.mcp_statuses.is_empty() {
            lines.push(Line::from(Span::styled(
                "No MCP servers configured.",
                muted(),
            )));
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "Declare stdio servers in .medusa/mcp.json:",
                value_style(),
            )));
            lines.push(Line::from(Span::styled(
                r#"  {"servers": {"docs": {"command": "npx", "args": ["-y", "some-mcp-server"]}}}"#,
                muted(),
            )));
            lines.push(Line::from(Span::styled(
                r#"  Add "readOnly": true to allow a side-effect-free server in readonly mode."#,
                muted(),
            )));
        }
        for status in &self.mcp_statuses {
            let (state_text, state_style) = match &status.state {
                McpServerStateLabel::Idle => ("not started".to_string(), muted()),
                McpServerStateLabel::Connecting => ("connecting…".to_string(), prompt_style()),
                McpServerStateLabel::Ready => (
                    format!(
                        "ready · {} tool{}",
                        status.tools.len(),
                        if status.tools.len() == 1 { "" } else { "s" }
                    ),
                    success_style(),
                ),
                McpServerStateLabel::Disconnected => ("disconnected".to_string(), error_style()),
                McpServerStateLabel::Failed(error) => {
                    (format!("failed · {}", truncate(error, 64)), error_style())
                }
            };
            let mut header = vec![
                Span::styled(
                    status.name.clone(),
                    value_style().add_modifier(Modifier::BOLD),
                ),
                Span::styled("  ", muted()),
                Span::styled(state_text, state_style),
            ];
            if status.read_only {
                header.push(Span::styled("  · read-only", muted()));
            }
            if status.restarts > 0 {
                header.push(Span::styled(
                    format!("  · {} restart(s)", status.restarts),
                    muted(),
                ));
            }
            lines.push(Line::from(header));
            lines.push(Line::from(vec![
                Span::styled("  $ ", prompt_style()),
                Span::styled(truncate(&status.command_line, 68), muted()),
            ]));
            for tool in status.tools.iter().take(6) {
                lines.push(Line::from(Span::styled(
                    format!("    {tool}"),
                    value_style(),
                )));
            }
            if status.tools.len() > 6 {
                lines.push(Line::from(Span::styled(
                    format!("    … +{} more tools", status.tools.len() - 6),
                    muted(),
                )));
            }
            if let Some(tail) = &status.stderr_tail
                && matches!(
                    status.state,
                    McpServerStateLabel::Failed(_) | McpServerStateLabel::Disconnected
                )
            {
                let recent: Vec<&str> = tail.lines().rev().take(3).collect();
                for line in recent.into_iter().rev() {
                    lines.push(Line::from(Span::styled(
                        format!("    stderr: {}", truncate(line, 64)),
                        error_style(),
                    )));
                }
            }
            lines.push(Line::from(""));
        }
        lines.push(Line::from(vec![
            Span::styled("config ", muted()),
            Span::styled(".medusa/mcp.json", prompt_style()),
            Span::styled("  ·  ", muted()),
            Span::styled("/mcp restart <server>", prompt_style()),
            Span::styled(" reconnects a failed server", muted()),
        ]));

        let paragraph = Paragraph::new(lines)
            .block(modal_block(" MCP servers "))
            .style(Style::default().bg(surface()).fg(text()))
            .wrap(Wrap { trim: false });
        frame.render_widget(paragraph, area);
    }

    pub(super) fn draw_image_preview_modal(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let attachments = self.image_attachments();
        let count = attachments.len();
        let selected = self.image_preview_index.min(count.saturating_sub(1));
        let title = if count == 0 {
            " Image Preview ".to_string()
        } else {
            format!(" Image Preview {}/{} ", selected + 1, count)
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(title)
            .border_style(Style::default().fg(accent_color()))
            .style(Style::default().bg(surface()).fg(text()));
        frame.render_widget(block, area);

        let inner = area.inner(Margin {
            horizontal: 2,
            vertical: 1,
        });
        if count == 0 || inner.height == 0 {
            let empty = Paragraph::new(vec![
                Line::from(Span::styled("No images attached yet.", muted())),
                Line::from(Span::styled(
                    "Paste one with Ctrl+I or drag an image path into the composer.",
                    value_style(),
                )),
            ])
            .style(Style::default().bg(surface()).fg(text()))
            .wrap(Wrap { trim: true });
            frame.render_widget(empty, inner);
            return;
        }

        let Some(attachment) = attachments.get(selected).cloned() else {
            return;
        };
        let image_input_warning = image_input_warning(self.model.provider_name());
        let header_height = if image_input_warning.is_some() { 3 } else { 2 };
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(header_height),
                Constraint::Min(4),
                Constraint::Length(1),
            ])
            .split(inner);
        let pending = self.current_preview_image_is_pending();
        let mut header_lines = vec![
            Line::from(vec![
                Span::styled(truncate(&attachment.name, 44), prompt_style()),
                Span::styled("  ", muted()),
                Span::styled(
                    format!("{}×{}", attachment.width, attachment.height),
                    value_style(),
                ),
                Span::styled("  ", muted()),
                Span::styled(human_bytes(attachment.size_bytes), muted()),
                Span::styled("  ", muted()),
                Span::styled(
                    if pending { "pending" } else { "sent" },
                    if pending { success_style() } else { muted() },
                ),
            ]),
            Line::from(vec![
                Span::styled(format!("zoom {}%", self.image_preview_zoom), accent()),
                Span::styled("  ", muted()),
                Span::styled(attachment.path.to_string_lossy().to_string(), muted()),
            ]),
        ];
        if let Some(warning) = image_input_warning {
            header_lines.push(Line::from(vec![
                Span::styled("preview only", prompt_style()),
                Span::styled(" · ", muted()),
                Span::styled(warning, muted()),
            ]));
        }
        let header = Paragraph::new(header_lines)
            .style(Style::default().bg(surface()).fg(text()))
            .wrap(Wrap { trim: true });
        frame.render_widget(header, sections[0]);

        let image_area = sections[1];
        let (preview_width, preview_height) =
            preview_image_dimensions(&attachment, image_area, self.image_preview_zoom);
        let x_offset = if preview_width < image_area.width {
            (image_area.width - preview_width) / 2
        } else {
            0
        };
        let y_offset = if preview_height < image_area.height {
            ((image_area.height - preview_height) / 2).min(i16::MAX as u16) as i16
        } else {
            0
        };
        let rendered = self.image_renderer.render(
            frame,
            &attachment,
            image_area,
            preview_width,
            preview_height,
            x_offset,
            y_offset,
        );
        if !rendered {
            let placeholder = Paragraph::new(image_placeholder_lines(
                &attachment,
                image_area.width.min(preview_width).max(10),
                image_area.height.min(preview_height).max(3),
            ))
            .alignment(Alignment::Center)
            .style(Style::default().bg(surface()).fg(text()))
            .wrap(Wrap { trim: false });
            frame.render_widget(placeholder, image_area);
        }

        let mut footer_spans = vec![
            Span::styled("j/k", prompt_style()),
            Span::styled(" image  ", muted()),
            Span::styled("+/-", prompt_style()),
            Span::styled(" zoom  ", muted()),
            Span::styled("0", prompt_style()),
            Span::styled(" reset  ", muted()),
        ];
        if pending {
            footer_spans.extend([
                Span::styled("d", prompt_style()),
                Span::styled(" detach  ", muted()),
            ]);
        }
        footer_spans.extend([
            Span::styled("o", prompt_style()),
            Span::styled(" open  ", muted()),
            Span::styled("y", prompt_style()),
            Span::styled(" copy path  ", muted()),
            Span::styled("esc", prompt_style()),
            Span::styled(" close", muted()),
        ]);
        let footer = Paragraph::new(Line::from(footer_spans))
            .alignment(Alignment::Right)
            .style(Style::default().bg(surface()));
        frame.render_widget(footer, sections[2]);
    }

    pub(super) fn draw_commands_modal(&self, frame: &mut Frame<'_>, area: Rect) {
        let rows = SLASH_COMMANDS.iter().map(|command| {
            Row::new(vec![
                Cell::from(command.category).style(muted()),
                Cell::from(command.name).style(prompt_style()),
                Cell::from(command.args).style(muted()),
                Cell::from(command.description).style(value_style()),
            ])
        });
        let table = Table::new(
            rows,
            [
                Constraint::Length(10),
                Constraint::Length(12),
                Constraint::Length(14),
                Constraint::Min(20),
            ],
        )
        .header(
            Row::new(vec!["group", "command", "args", "description"])
                .style(muted().add_modifier(Modifier::BOLD)),
        )
        .block(modal_block(" Commands "))
        .column_spacing(1);

        frame.render_widget(table, area);
    }

    pub(super) fn draw_settings_modal(&self, frame: &mut Frame<'_>, area: Rect) {
        let items = self.settings_items();
        let selected = self.settings_selection.min(items.len().saturating_sub(1));
        frame.render_widget(
            modal_block(" Settings ")
                .border_type(BorderType::Rounded)
                .padding(Padding::new(2, 2, 0, 0)),
            area,
        );

        let inner = area.inner(Margin {
            horizontal: 3,
            vertical: 1,
        });
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Min(6),
                Constraint::Length(1),
            ])
            .split(inner);
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(34), Constraint::Min(30)])
            .split(sections[1]);

        let header = Paragraph::new(vec![
            Line::from(vec![
                Span::styled("MEDUSA", accent().add_modifier(Modifier::BOLD)),
                Span::styled(" settings", value_style().add_modifier(Modifier::BOLD)),
            ]),
            Line::from(vec![
                Span::styled("command-surface controls", muted()),
                Span::styled(" · ", muted()),
                Span::styled("enter opens editable rows", prompt_style()),
            ]),
        ])
        .style(Style::default().bg(surface()).fg(text()));
        frame.render_widget(header, sections[0]);

        let rows = items
            .iter()
            .map(|item| {
                let marker = if item.editable { "● " } else { "· " };
                ListItem::new(Line::from(vec![
                    Span::styled(
                        marker,
                        if item.editable {
                            success_style()
                        } else {
                            muted()
                        },
                    ),
                    Span::styled(format!("{:<14}", item.key), value_style()),
                    Span::styled(truncate(&item.value, 15), muted()),
                ]))
            })
            .collect::<Vec<_>>();
        let mut state = ListState::default().with_selected(Some(selected));
        let list = List::new(rows)
            .style(Style::default().bg(surface()).fg(text()))
            .highlight_style(command_selected_style())
            .highlight_symbol("▌ ");
        frame.render_stateful_widget(list, columns[0], &mut state);

        if let Some(item) = items.get(selected) {
            let mut detail = vec![
                Line::from(vec![
                    Span::styled(item.key, prompt_style()),
                    Span::styled("  ", muted()),
                    Span::styled(&item.value, value_style().add_modifier(Modifier::BOLD)),
                ]),
                Line::from(""),
                Line::from(Span::styled(item.description, value_style())),
                Line::from(""),
                Line::from(vec![
                    Span::styled("action  ", muted()),
                    Span::styled(
                        item.action,
                        if item.editable {
                            prompt_style()
                        } else {
                            muted()
                        },
                    ),
                ]),
            ];
            if item.key == "theme" {
                detail.push(Line::from(""));
                detail.extend(theme_preview_lines(self.theme));
            } else if item.key == "model" {
                detail.push(Line::from(""));
                detail.extend(model_picker_detail_lines(
                    self.model.model_name(),
                    self.model.reasoning_effort(),
                    self.model.model_name(),
                    self.model.reasoning_effort(),
                ));
            } else if item.key == "reasoning" {
                detail.push(Line::from(""));
                detail.extend(reasoning_detail_lines(
                    self.model.model_name(),
                    self.model.reasoning_effort(),
                    self.model.reasoning_effort(),
                ));
            } else if item.key == "permissions" {
                detail.push(Line::from(""));
                detail.extend(permission_detail_lines(
                    self.permission_mode,
                    self.permission_mode,
                ));
            }

            let detail = Paragraph::new(detail)
                .style(Style::default().bg(surface()).fg(text()))
                .wrap(Wrap { trim: true });
            frame.render_widget(detail, columns[1]);
        }

        let footer = Paragraph::new(Line::from(vec![
            Span::styled("↑/↓ tab", prompt_style()),
            Span::styled(" select  ", muted()),
            Span::styled("enter", prompt_style()),
            Span::styled(" edit  ", muted()),
            Span::styled("esc", prompt_style()),
            Span::styled(" close", muted()),
        ]))
        .alignment(Alignment::Right)
        .style(Style::default().bg(surface()));
        frame.render_widget(footer, sections[2]);
    }

    pub(super) fn draw_jobs_modal(&self, frame: &mut Frame<'_>, area: Rect) {
        let rows = self.background_jobs.values().rev().map(|job| {
            let elapsed = job
                .finished_at
                .unwrap_or_else(Instant::now)
                .saturating_duration_since(job.started_at);
            let status = match job.state {
                ToolRunState::Running => format!("running · {}s", elapsed.as_secs()),
                ToolRunState::Succeeded => format!(
                    "done · exit {} · {}s",
                    job.exit_code.unwrap_or(0),
                    elapsed.as_secs()
                ),
                ToolRunState::Failed => format!(
                    "failed · exit {} · {}s",
                    job.exit_code.unwrap_or(-1),
                    elapsed.as_secs()
                ),
            };
            Row::new(vec![
                Cell::from(job.id.clone()).style(prompt_style()),
                Cell::from(job.pid.to_string()).style(muted()),
                Cell::from(status).style(tool_output_style(job.state)),
                Cell::from(truncate(
                    &format!(
                        "{} · {}",
                        job.command,
                        abbreviate_home(&job.cwd.to_string_lossy())
                    ),
                    64,
                ))
                .style(value_style()),
            ])
        });
        let table = Table::new(
            rows,
            [
                Constraint::Length(16),
                Constraint::Length(8),
                Constraint::Length(22),
                Constraint::Min(24),
            ],
        )
        .header(
            Row::new(vec!["id", "pid", "status", "command"])
                .style(muted().add_modifier(Modifier::BOLD)),
        )
        .block(modal_block(" Background jobs · /kill /tail /restart "))
        .column_spacing(1);
        frame.render_widget(table, area);
    }

    pub(super) fn draw_models_modal(&self, frame: &mut Frame<'_>, area: Rect) {
        frame.render_widget(
            modal_block(" Model & execution mode ")
                .border_type(BorderType::Rounded)
                .padding(Padding::new(2, 2, 0, 0)),
            area,
        );
        let inner = area.inner(Margin {
            horizontal: 3,
            vertical: 1,
        });
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Min(5),
                Constraint::Length(1),
            ])
            .split(inner);
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(28),
                Constraint::Length(2),
                Constraint::Length(18),
                Constraint::Length(2),
                Constraint::Min(28),
            ])
            .split(sections[1]);

        let header = Paragraph::new(vec![
            Line::from(vec![
                Span::styled(
                    "Model & execution mode",
                    accent().add_modifier(Modifier::BOLD),
                ),
                Span::styled("  ", muted()),
                Span::styled(self.model.provider_name(), muted()),
                Span::styled("/", muted()),
                Span::styled(self.model.model_name().to_string(), prompt_style()),
                Span::styled(" · ", muted()),
                Span::styled(self.model.reasoning_effort().to_string(), prompt_style()),
            ]),
            Line::from(vec![
                Span::styled("choose effort or Ultra orchestration", muted()),
                Span::styled("  ·  ", muted()),
                Span::styled("/model <id>", prompt_style()),
                Span::styled(" accepts any model id", muted()),
            ]),
        ])
        .style(Style::default().bg(surface()).fg(text()));
        frame.render_widget(header, sections[0]);

        let model_sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(1)])
            .split(columns[0]);
        let reasoning_sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(1)])
            .split(columns[2]);
        let detail_sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(1)])
            .split(columns[4]);
        let model_pane_active = self.model_picker_pane == ModelPickerPane::Models;
        let reasoning_pane_active = self.model_picker_pane == ModelPickerPane::Reasoning;

        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    if model_pane_active { "● " } else { "  " },
                    if model_pane_active { accent() } else { muted() },
                ),
                Span::styled(
                    "MODEL",
                    if model_pane_active {
                        accent().add_modifier(Modifier::BOLD)
                    } else {
                        muted().add_modifier(Modifier::BOLD)
                    },
                ),
            ]))
            .style(Style::default().bg(surface())),
            model_sections[0],
        );
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    if reasoning_pane_active { "● " } else { "  " },
                    if reasoning_pane_active {
                        accent()
                    } else {
                        muted()
                    },
                ),
                Span::styled(
                    "EFFORT / MODE",
                    if reasoning_pane_active {
                        accent().add_modifier(Modifier::BOLD)
                    } else {
                        muted().add_modifier(Modifier::BOLD)
                    },
                ),
            ]))
            .style(Style::default().bg(surface())),
            reasoning_sections[0],
        );
        frame.render_widget(
            Paragraph::new(Span::styled(
                "SELECTION",
                muted().add_modifier(Modifier::BOLD),
            ))
            .style(Style::default().bg(surface())),
            detail_sections[0],
        );

        let choices = model_choices(self.model.model_name());
        let rows = choices
            .iter()
            .map(|model| {
                let active = model == self.model.model_name();
                let (label, _) = model_display(model);
                let mut spans = vec![
                    Span::styled(
                        if active { "● " } else { "  " },
                        if active { success_style() } else { muted() },
                    ),
                    Span::styled(label.clone(), value_style()),
                ];
                // Show the wire slug beside a friendlier display name.
                if label != *model {
                    spans.push(Span::styled(format!("  {model}"), muted()));
                }
                ListItem::new(Line::from(spans))
            })
            .collect::<Vec<_>>();
        let mut state = ListState::default().with_selected(Some(self.model_selection));
        let list = List::new(rows)
            .style(Style::default().bg(surface()).fg(text()))
            .highlight_style(if model_pane_active {
                command_selected_style()
            } else {
                Style::default()
                    .bg(surface())
                    .fg(accent_color())
                    .add_modifier(Modifier::BOLD)
            })
            .highlight_symbol(if model_pane_active { "▌ " } else { "› " });
        frame.render_stateful_widget(list, model_sections[1], &mut state);

        let selected = choices
            .get(self.model_selection.min(choices.len().saturating_sub(1)))
            .map(String::as_str)
            .unwrap_or(self.model.model_name());
        let reasoning_choices = reasoning_choices(selected, "");
        let selected_reasoning = reasoning_choices
            .get(
                self.reasoning_selection
                    .min(reasoning_choices.len().saturating_sub(1)),
            )
            .map(String::as_str)
            .unwrap_or(self.model.reasoning_effort());
        let reasoning_rows = reasoning_choices
            .iter()
            .map(|effort| {
                let active =
                    selected == self.model.model_name() && effort == self.model.reasoning_effort();
                ListItem::new(Line::from(vec![
                    Span::styled(
                        if active { "● " } else { "  " },
                        if active { success_style() } else { muted() },
                    ),
                    Span::styled(effort.clone(), value_style()),
                ]))
            })
            .collect::<Vec<_>>();
        let mut reasoning_state =
            ListState::default().with_selected(Some(self.reasoning_selection));
        let reasoning_list = List::new(reasoning_rows)
            .style(Style::default().bg(surface()).fg(text()))
            .highlight_style(if reasoning_pane_active {
                command_selected_style()
            } else {
                Style::default()
                    .bg(surface())
                    .fg(accent_color())
                    .add_modifier(Modifier::BOLD)
            })
            .highlight_symbol(if reasoning_pane_active {
                "▌ "
            } else {
                "› "
            });
        frame.render_stateful_widget(reasoning_list, reasoning_sections[1], &mut reasoning_state);

        let detail = Paragraph::new(model_picker_detail_lines(
            selected,
            selected_reasoning,
            self.model.model_name(),
            self.model.reasoning_effort(),
        ))
        .style(Style::default().bg(surface()).fg(text()))
        .wrap(Wrap { trim: true });
        frame.render_widget(detail, detail_sections[1]);

        let footer = Paragraph::new(Line::from(vec![
            Span::styled("↑/↓", prompt_style()),
            Span::styled(" choose  ", muted()),
            Span::styled("←/→ tab", prompt_style()),
            Span::styled(" pane  ", muted()),
            Span::styled("enter", prompt_style()),
            Span::styled(
                if model_pane_active {
                    " next  "
                } else {
                    " save  "
                },
                muted(),
            ),
            Span::styled("esc", prompt_style()),
            Span::styled(" close", muted()),
        ]))
        .alignment(Alignment::Right)
        .style(Style::default().bg(surface()));
        frame.render_widget(footer, sections[2]);
    }

    pub(super) fn draw_reasoning_modal(&self, frame: &mut Frame<'_>, area: Rect) {
        frame.render_widget(
            modal_block(" Reasoning & orchestration ")
                .border_type(BorderType::Rounded)
                .padding(Padding::new(2, 2, 0, 0)),
            area,
        );
        let inner = area.inner(Margin {
            horizontal: 3,
            vertical: 1,
        });
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Min(5),
                Constraint::Length(1),
            ])
            .split(inner);
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(22), Constraint::Min(34)])
            .split(sections[1]);

        let model = self.model.model_name().to_string();
        let active = self.model.reasoning_effort().to_string();
        let (model_label, _) = model_display(&model);
        let header = Paragraph::new(vec![
            Line::from(vec![
                Span::styled(
                    "Reasoning & orchestration",
                    accent().add_modifier(Modifier::BOLD),
                ),
                Span::styled("  for ", muted()),
                Span::styled(model_label, prompt_style()),
            ]),
            Line::from(Span::styled(
                "effort controls thinking depth; Ultra adds proactive multi-agent delegation",
                muted(),
            )),
        ])
        .style(Style::default().bg(surface()).fg(text()));
        frame.render_widget(header, sections[0]);

        let choices = reasoning_choices(&model, &active);
        let rows = choices
            .iter()
            .map(|effort| {
                let is_active = *effort == active;
                ListItem::new(Line::from(vec![
                    Span::styled(
                        if is_active { "● " } else { "  " },
                        if is_active { success_style() } else { muted() },
                    ),
                    Span::styled(effort.clone(), value_style()),
                ]))
            })
            .collect::<Vec<_>>();
        let mut state = ListState::default().with_selected(Some(self.reasoning_selection));
        let list = List::new(rows)
            .style(Style::default().bg(surface()).fg(text()))
            .highlight_style(command_selected_style())
            .highlight_symbol("▌ ");
        frame.render_stateful_widget(list, columns[0], &mut state);

        let selected = choices
            .get(
                self.reasoning_selection
                    .min(choices.len().saturating_sub(1)),
            )
            .map(String::as_str)
            .unwrap_or(active.as_str());
        let detail = Paragraph::new(reasoning_detail_lines(&model, selected, &active))
            .style(Style::default().bg(surface()).fg(text()))
            .wrap(Wrap { trim: true });
        frame.render_widget(detail, columns[1]);

        let footer = Paragraph::new(Line::from(vec![
            Span::styled("↑/↓ tab", prompt_style()),
            Span::styled(" choose  ", muted()),
            Span::styled("enter", prompt_style()),
            Span::styled(" save  ", muted()),
            Span::styled("esc", prompt_style()),
            Span::styled(" close", muted()),
        ]))
        .alignment(Alignment::Right)
        .style(Style::default().bg(surface()));
        frame.render_widget(footer, sections[2]);
    }

    pub(super) fn draw_permissions_modal(&self, frame: &mut Frame<'_>, area: Rect) {
        frame.render_widget(
            modal_block(" Permissions ")
                .border_type(BorderType::Rounded)
                .padding(Padding::new(2, 2, 0, 0)),
            area,
        );
        let inner = area.inner(Margin {
            horizontal: 3,
            vertical: 1,
        });
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Min(5),
                Constraint::Length(1),
            ])
            .split(inner);
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(30), Constraint::Min(34)])
            .split(sections[1]);

        let header = Paragraph::new(vec![
            Line::from(vec![
                Span::styled("Permission mode", accent().add_modifier(Modifier::BOLD)),
                Span::styled("  ", muted()),
                Span::styled(self.permission_mode.label(), prompt_style()),
            ]),
            Line::from(vec![
                Span::styled("writes ", muted()),
                Span::styled(".medusa/permissions.json", value_style()),
                Span::styled(" and refreshes tools immediately", muted()),
            ]),
        ])
        .style(Style::default().bg(surface()).fg(text()));
        frame.render_widget(header, sections[0]);

        let rows = PermissionMode::all()
            .iter()
            .map(|mode| {
                let active = *mode == self.permission_mode;
                ListItem::new(Line::from(vec![
                    Span::styled(
                        if active { "● " } else { "  " },
                        if active { success_style() } else { muted() },
                    ),
                    Span::styled(format!("{:<12}", mode.label()), value_style()),
                    Span::styled(mode.name(), muted()),
                ]))
            })
            .collect::<Vec<_>>();
        let mut state = ListState::default().with_selected(Some(self.permission_selection));
        let list = List::new(rows)
            .style(Style::default().bg(surface()).fg(text()))
            .highlight_style(command_selected_style())
            .highlight_symbol("▌ ");
        frame.render_stateful_widget(list, columns[0], &mut state);

        let selected = PermissionMode::all()[self
            .permission_selection
            .min(PermissionMode::all().len().saturating_sub(1))];
        let detail = Paragraph::new(permission_detail_lines(selected, self.permission_mode))
            .style(Style::default().bg(surface()).fg(text()))
            .wrap(Wrap { trim: true });
        frame.render_widget(detail, columns[1]);

        let footer = Paragraph::new(Line::from(vec![
            Span::styled("↑/↓ tab", prompt_style()),
            Span::styled(" choose  ", muted()),
            Span::styled("enter", prompt_style()),
            Span::styled(" save  ", muted()),
            Span::styled("esc", prompt_style()),
            Span::styled(" close", muted()),
        ]))
        .alignment(Alignment::Right)
        .style(Style::default().bg(surface()));
        frame.render_widget(footer, sections[2]);
    }

    pub(super) fn draw_themes_modal(&self, frame: &mut Frame<'_>, area: Rect) {
        frame.render_widget(
            modal_block(" Themes ")
                .border_type(BorderType::Rounded)
                .padding(Padding::new(2, 2, 0, 0)),
            area,
        );
        let inner = area.inner(Margin {
            horizontal: 3,
            vertical: 1,
        });
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Min(6),
                Constraint::Length(1),
            ])
            .split(inner);
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(30), Constraint::Min(34)])
            .split(sections[1]);

        let previewing = self
            .theme_preview_original
            .is_some_and(|original| original != self.theme);
        let header = Paragraph::new(vec![
            Line::from(vec![
                Span::styled("Theme picker", accent().add_modifier(Modifier::BOLD)),
                Span::styled("  ", muted()),
                Span::styled(
                    if previewing {
                        "live preview"
                    } else {
                        "choose theme"
                    },
                    if previewing { success_style() } else { muted() },
                ),
            ]),
            Line::from(vec![
                Span::styled("cycles repaint immediately", muted()),
                Span::styled(" · ", muted()),
                Span::styled("enter saves", prompt_style()),
                Span::styled(" · ", muted()),
                Span::styled("esc cancels", prompt_style()),
            ]),
        ])
        .style(Style::default().bg(surface()).fg(text()));
        frame.render_widget(header, sections[0]);

        let rows = ThemeKind::all()
            .iter()
            .map(|theme| {
                let state = if *theme == self.theme { "● " } else { "  " };
                ListItem::new(Line::from(vec![
                    Span::styled(
                        state,
                        if *theme == self.theme {
                            success_style()
                        } else {
                            muted()
                        },
                    ),
                    Span::styled(format!("{:<18}", theme.label()), value_style()),
                    Span::styled("██", Style::default().fg(theme.palette().accent)),
                ]))
            })
            .collect::<Vec<_>>();
        let mut state = ListState::default().with_selected(Some(self.theme_selection));
        let list = List::new(rows)
            .style(Style::default().bg(surface()).fg(text()))
            .highlight_style(command_selected_style())
            .highlight_symbol("▌ ");
        frame.render_stateful_widget(list, columns[0], &mut state);

        let selected_theme = ThemeKind::all()[self.theme_selection.min(ThemeKind::all().len() - 1)];
        let mut detail = vec![
            Line::from(vec![
                Span::styled(selected_theme.label(), prompt_style()),
                Span::styled("  ", muted()),
                Span::styled(selected_theme.name(), muted()),
            ]),
            Line::from(""),
            Line::from(Span::styled(selected_theme.description(), value_style())),
            Line::from(""),
        ];
        detail.extend(theme_preview_lines(selected_theme));
        detail.push(Line::from(""));
        detail.push(Line::from(vec![
            Span::styled("status  ", muted()),
            Span::styled(
                if previewing {
                    "previewing live; not saved yet"
                } else {
                    "saved"
                },
                if previewing {
                    prompt_style()
                } else {
                    success_style()
                },
            ),
        ]));
        let detail = Paragraph::new(detail)
            .style(Style::default().bg(surface()).fg(text()))
            .wrap(Wrap { trim: true });
        frame.render_widget(detail, columns[1]);

        let footer = Paragraph::new(Line::from(vec![
            Span::styled("↑/↓ tab", prompt_style()),
            Span::styled(" preview  ", muted()),
            Span::styled("enter", prompt_style()),
            Span::styled(" save  ", muted()),
            Span::styled("esc", prompt_style()),
            Span::styled(" cancel", muted()),
        ]))
        .alignment(Alignment::Right)
        .style(Style::default().bg(surface()));
        frame.render_widget(footer, sections[2]);
    }

    pub(super) fn draw_help_modal(&self, frame: &mut Frame<'_>, area: Rect) {
        let help = Paragraph::new(vec![
            Line::from(vec![Span::styled("Slash commands", prompt_style())]),
            Line::from("Type / or press Ctrl+P to open the command palette."),
            Line::from("Use ↑/↓, PgUp/PgDn, Home/End, Enter, and Esc."),
            Line::from(""),
            Line::from(vec![Span::styled("Chat", prompt_style())]),
            Line::from("Enter sends. Shift+Enter inserts a newline."),
            Line::from("Ctrl+↑/↓ or mouse wheel scrolls chat."),
            Line::from("Ctrl+I attaches an image. Ctrl+O previews. Ctrl+D detaches latest."),
            Line::from(""),
            Line::from(vec![Span::styled("Themes", prompt_style())]),
            Line::from("Use /theme to browse themes or /theme opencode to switch directly."),
            Line::from(""),
            Line::from(vec![Span::styled("Modals", prompt_style())]),
            Line::from(
                "/plan or shift+tab toggles plan mode. Select a tool call with j/k, enter expands it.",
            ),
            Line::from("Esc or Enter closes simple popups."),
            Line::from(""),
            Line::from("Try /fork before risky work, or /tree to inspect branches."),
        ])
        .block(modal_block(" Help "))
        .wrap(Wrap { trim: false });
        frame.render_widget(help, area);
    }

    pub(super) fn draw_workflows_modal(&self, frame: &mut Frame<'_>, area: Rect) {
        if self.workflows.is_empty() {
            let empty = Paragraph::new(vec![
                Line::from(Span::styled("No workflow runs yet.", muted())),
                Line::from(""),
                Line::from(vec![
                    Span::styled("Run ", muted()),
                    Span::styled("/workflow <task>", prompt_style()),
                    Span::styled(" for larger jobs that need subagents.", muted()),
                ]),
            ])
            .block(modal_block(" Workflows "))
            .wrap(Wrap { trim: true });
            frame.render_widget(empty, area);
            return;
        }

        let rows = self.workflows.iter().rev().take(12).map(|workflow| {
            let progress = workflow_progress(workflow);
            Row::new(vec![
                Cell::from(workflow_state_label(workflow.status))
                    .style(workflow_state_style(workflow.status)),
                Cell::from(truncate(&workflow.title, 34)).style(value_style()),
                Cell::from(workflow_progress_label(progress)).style(muted()),
                Cell::from(workflow_latest_activity(workflow))
                    .style(workflow_activity_style(workflow)),
            ])
        });

        let table = Table::new(
            rows,
            [
                Constraint::Length(10),
                Constraint::Min(26),
                Constraint::Length(24),
                Constraint::Min(26),
            ],
        )
        .header(
            Row::new(vec!["status", "workflow", "progress", "latest"])
                .style(muted().add_modifier(Modifier::BOLD)),
        )
        .block(modal_block(" Workflows "))
        .column_spacing(2);

        frame.render_widget(table, area);
    }

    pub(super) fn draw_session_tree_modal(&self, frame: &mut Frame<'_>, area: Rect) {
        let entries = self
            .session
            .as_ref()
            .map(SessionStore::tree_entries)
            .unwrap_or_default();
        let rows = entries.into_iter().take(14).map(|entry| {
            let branch = if entry.depth == 0 { "●" } else { "└" };
            let indent = "  ".repeat(entry.depth);
            let name = format!("{indent}{branch} {}", entry.name);
            Row::new(vec![
                Cell::from(name).style(if entry.current {
                    prompt_style()
                } else {
                    value_style()
                }),
                Cell::from(entry.parent).style(muted()),
                Cell::from(entry.size).style(muted()),
                Cell::from(if entry.current { "yes" } else { "" }).style(prompt_style()),
            ])
        });
        let table = Table::new(
            rows,
            [
                Constraint::Min(28),
                Constraint::Length(18),
                Constraint::Length(10),
                Constraint::Length(8),
            ],
        )
        .header(
            Row::new(vec!["tree", "parent", "size", "current"])
                .style(muted().add_modifier(Modifier::BOLD)),
        )
        .block(modal_block(" Session Tree "))
        .column_spacing(2);
        frame.render_widget(table, area);
    }

    pub(super) fn draw_sessions_modal(&self, frame: &mut Frame<'_>, area: Rect) {
        let sessions = self
            .session
            .as_ref()
            .map(SessionStore::list_sessions)
            .unwrap_or_default();
        let rows = sessions.into_iter().take(10).map(|session| {
            Row::new(vec![
                Cell::from(session.name).style(value_style()),
                Cell::from(session.parent).style(muted()),
                Cell::from(session.size).style(muted()),
                Cell::from(session.current).style(prompt_style()),
            ])
        });
        let table = Table::new(
            rows,
            [
                Constraint::Min(24),
                Constraint::Length(18),
                Constraint::Length(10),
                Constraint::Length(8),
            ],
        )
        .header(
            Row::new(vec!["session", "parent", "size", "current"])
                .style(muted().add_modifier(Modifier::BOLD)),
        )
        .block(modal_block(" Sessions "))
        .column_spacing(2);
        frame.render_widget(table, area);
    }

    pub(super) fn draw_rewind_modal(&self, frame: &mut Frame<'_>, area: Rect) {
        match self.rewind_stage {
            RewindStage::Pick => self.draw_rewind_pick(frame, area),
            RewindStage::Confirm => self.draw_rewind_confirm(frame, area),
        }
    }

    pub(super) fn draw_rewind_pick(&self, frame: &mut Frame<'_>, area: Rect) {
        frame.render_widget(modal_block(" Rewind "), area);
        let inner = area.inner(Margin {
            horizontal: 2,
            vertical: 1,
        });
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Min(4),
                Constraint::Length(1),
            ])
            .split(inner);

        let current_session = self
            .session
            .as_ref()
            .map(SessionStore::current_id)
            .unwrap_or_default();
        let header = Paragraph::new(vec![
            Line::from(Span::styled(
                "Restore files to the state before a turn",
                accent().add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                "Edit/patch tool changes only — shell-command changes are not rewound.",
                muted(),
            )),
        ])
        .style(Style::default().bg(surface()).fg(text()));
        frame.render_widget(header, sections[0]);

        let rows = self
            .rewind_entries
            .iter()
            .map(|entry| {
                let prompt = if entry.prompt_excerpt.is_empty() {
                    "(no prompt)".to_string()
                } else {
                    truncate(&entry.prompt_excerpt, 40)
                };
                let session_tag = if entry.session_id == current_session {
                    "this session".to_string()
                } else {
                    compact_session_id(&entry.session_id)
                };
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{:>8}", time_ago_ms(entry.created_at_ms)), muted()),
                    Span::styled("  ", muted()),
                    Span::styled(format!("{prompt:<43}"), value_style()),
                    Span::styled(
                        format!(
                            "{} file{}",
                            entry.files.len(),
                            if entry.files.len() == 1 { "" } else { "s" }
                        ),
                        prompt_style(),
                    ),
                    Span::styled("  ", muted()),
                    Span::styled(session_tag, muted()),
                ]))
            })
            .collect::<Vec<_>>();
        let mut state = ListState::default().with_selected(Some(self.rewind_selection));
        let list = List::new(rows)
            .style(Style::default().bg(surface()).fg(text()))
            .highlight_style(command_selected_style())
            .highlight_symbol("▌ ");
        frame.render_stateful_widget(list, sections[1], &mut state);

        let footer = Paragraph::new(Line::from(vec![
            Span::styled("↑/↓", prompt_style()),
            Span::styled(" select  ", muted()),
            Span::styled("enter", prompt_style()),
            Span::styled(" review  ", muted()),
            Span::styled("esc", prompt_style()),
            Span::styled(" close", muted()),
        ]))
        .alignment(Alignment::Right)
        .style(Style::default().bg(surface()));
        frame.render_widget(footer, sections[2]);
    }

    pub(super) fn draw_rewind_confirm(&self, frame: &mut Frame<'_>, area: Rect) {
        frame.render_widget(modal_block(" Rewind · Confirm "), area);
        let inner = area.inner(Margin {
            horizontal: 2,
            vertical: 1,
        });
        let Some(entry) = self.selected_rewind_entry() else {
            return;
        };
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(6),
                Constraint::Min(2),
                Constraint::Length(1),
            ])
            .split(inner);

        let skipped_too_large = entry
            .files
            .iter()
            .filter(|file| {
                matches!(
                    file.pre,
                    medusa_core::checkpoint::PreImageKind::SkippedTooLarge
                        | medusa_core::checkpoint::PreImageKind::SkippedSymlink
                )
            })
            .count();
        let mut summary_lines = vec![
            Line::from(vec![
                Span::styled("Rewind to before: ", muted()),
                Span::styled(truncate(&entry.prompt_excerpt, 56), prompt_style()),
            ]),
            Line::from(vec![Span::styled(
                format!(
                    "{} · {} file{}",
                    time_ago_ms(entry.created_at_ms),
                    entry.files.len(),
                    if entry.files.len() == 1 { "" } else { "s" }
                ),
                value_style(),
            )]),
            Line::from(Span::styled(
                "Restores files changed by edit/patch tools in this and all newer turns.",
                muted(),
            )),
            Line::from(Span::styled(
                "Shell-command changes are NOT rewound and may leave mixed state.",
                error_preview_style(),
            )),
            Line::from(Span::styled(
                "A pre-rewind safety checkpoint is created first, so rewind is undoable.",
                muted(),
            )),
        ];
        if skipped_too_large > 0 {
            summary_lines.push(Line::from(Span::styled(
                format!("{skipped_too_large} file(s) could not be captured (too large or symlink) and will not be rewound."),
                error_preview_style(),
            )));
        }
        let summary = Paragraph::new(summary_lines)
            .style(Style::default().bg(surface()).fg(text()))
            .wrap(Wrap { trim: true });
        frame.render_widget(summary, sections[0]);

        let rows = self
            .rewind_confirm_options()
            .into_iter()
            .map(|option| ListItem::new(Line::from(Span::styled(option, value_style()))))
            .collect::<Vec<_>>();
        let mut state = ListState::default().with_selected(Some(self.rewind_confirm_selection));
        let list = List::new(rows)
            .style(Style::default().bg(surface()).fg(text()))
            .highlight_style(command_selected_style())
            .highlight_symbol("▌ ");
        frame.render_stateful_widget(list, sections[1], &mut state);

        let footer = Paragraph::new(Line::from(vec![
            Span::styled("↑/↓", prompt_style()),
            Span::styled(" choose  ", muted()),
            Span::styled("enter", prompt_style()),
            Span::styled(" confirm  ", muted()),
            Span::styled("esc", prompt_style()),
            Span::styled(" back", muted()),
        ]))
        .alignment(Alignment::Right)
        .style(Style::default().bg(surface()));
        frame.render_widget(footer, sections[2]);
    }

    /// `/edit`: pick a previous user message to backtrack to. The current
    /// timeline is forked into the session tree before truncation, so
    /// nothing is lost — /tree can revisit it.
    pub(super) fn draw_edit_message_modal(&self, frame: &mut Frame<'_>, area: Rect) {
        frame.render_widget(modal_block(" Edit previous message "), area);
        let inner = area.inner(Margin {
            horizontal: 2,
            vertical: 1,
        });
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Min(4),
                Constraint::Length(1),
            ])
            .split(inner);

        let header = Paragraph::new(vec![
            Line::from(Span::styled(
                "Resend the conversation from an earlier message",
                accent().add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                "The current timeline is kept as a fork — browse it later with /tree.",
                muted(),
            )),
        ])
        .style(Style::default().bg(surface()).fg(text()));
        frame.render_widget(header, sections[0]);

        let rows = self
            .edit_picker_entries
            .iter()
            .enumerate()
            .map(|(ordinal, entry)| {
                let age = if ordinal == 0 {
                    "latest".to_string()
                } else {
                    format!("{} back", ordinal + 1)
                };
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{age:>8}"), muted()),
                    Span::styled("  ", muted()),
                    Span::styled(entry.preview.clone(), value_style()),
                ]))
            })
            .collect::<Vec<_>>();
        let mut state = ListState::default().with_selected(Some(self.edit_picker_selection));
        let list = List::new(rows)
            .style(Style::default().bg(surface()).fg(text()))
            .highlight_style(command_selected_style())
            .highlight_symbol("▌ ");
        frame.render_stateful_widget(list, sections[1], &mut state);

        let footer = Paragraph::new(Line::from(vec![
            Span::styled("↑/↓", prompt_style()),
            Span::styled(" select  ", muted()),
            Span::styled("enter", prompt_style()),
            Span::styled(" edit & resend  ", muted()),
            Span::styled("esc", prompt_style()),
            Span::styled(" close", muted()),
        ]))
        .alignment(Alignment::Right)
        .style(Style::default().bg(surface()));
        frame.render_widget(footer, sections[2]);
    }

    pub(super) fn settings_rows(&self) -> Vec<(&'static str, String)> {
        self.settings_items()
            .into_iter()
            .map(|item| (item.key, item.value))
            .collect()
    }

    pub(super) fn settings_items(&self) -> Vec<SettingsItem> {
        vec![
            SettingsItem {
                key: "model",
                value: self.model.model_name().to_string(),
                description: "The model Medusa sends coding turns to.",
                action: "enter opens model + execution mode picker",
                editable: true,
            },
            SettingsItem {
                key: "reasoning",
                value: self.model.reasoning_effort().to_string(),
                description: "Thinking depth, or Ultra for proactive multi-agent orchestration.",
                action: "enter opens reasoning picker",
                editable: true,
            },
            SettingsItem {
                key: "theme",
                value: self.theme.name().to_string(),
                description: "Terminal color palette for Medusa surfaces, markdown, prompts, and tool activity.",
                action: "enter opens live theme picker",
                editable: true,
            },
            SettingsItem {
                key: "permissions",
                value: self.permission_mode.name().to_string(),
                description: "Preset controlling terminal commands and file mutation tools.",
                action: "enter opens permission mode picker",
                editable: true,
            },
            SettingsItem {
                key: "bell",
                value: if self.bell_setting { "on" } else { "off" }.to_string(),
                description: "Terminal bell when a long turn finishes or needs approval (MEDUSA_BELL=off overrides).",
                action: "enter toggles",
                editable: true,
            },
            SettingsItem {
                key: "workspace",
                value: self.cwd_display.clone(),
                description: "Project root used by the harness, tools, sessions, and workflows.",
                action: "launch Medusa from another directory to change it",
                editable: false,
            },
            SettingsItem {
                key: "git",
                value: if self.inside_git_repo {
                    "enabled"
                } else {
                    "not detected"
                }
                .to_string(),
                description: "Whether the current workspace has a Git repository.",
                action: "read only",
                editable: false,
            },
            SettingsItem {
                key: "session",
                value: if self.session.is_some() {
                    "enabled"
                } else {
                    "disabled"
                }
                .to_string(),
                description: "Session transcript persistence for resume and fork flows.",
                action: "/sessions or /tree",
                editable: false,
            },
            SettingsItem {
                key: "streaming",
                value: if self.is_working() { "active" } else { "idle" }.to_string(),
                description: "Current model stream state.",
                action: "read only",
                editable: false,
            },
            SettingsItem {
                key: "workflows",
                value: format!(
                    "{} total · {} active",
                    self.workflows.len(),
                    self.workflow_events.len()
                ),
                description: "Background workflow/subagent runs tracked by the TUI.",
                action: "/workflows",
                editable: false,
            },
            SettingsItem {
                key: "queued turns",
                value: self.queued_turns.len().to_string(),
                description: "User turns waiting behind active work.",
                action: "read only",
                editable: false,
            },
            SettingsItem {
                key: "background jobs",
                value: format!(
                    "{} total · {} running",
                    self.background_jobs.len(),
                    self.background_jobs
                        .values()
                        .filter(|job| job.state == ToolRunState::Running)
                        .count()
                ),
                description: "Detached terminal jobs started by Medusa.",
                action: "/jobs",
                editable: false,
            },
            SettingsItem {
                key: "uptime",
                value: format!("{}s", self.started_at.elapsed().as_secs()),
                description: "How long this TUI process has been open.",
                action: "read only",
                editable: false,
            },
            SettingsItem {
                key: "turn time",
                value: self
                    .turn_started_at
                    .map(|t| format!("{}s", t.elapsed().as_secs()))
                    .unwrap_or_else(|| "idle".to_string()),
                description: "Elapsed time for the active model turn.",
                action: "read only",
                editable: false,
            },
            SettingsItem {
                key: "scrollback",
                value: self.chat_scroll.to_string(),
                description: "Current transcript scroll offset from the bottom.",
                action: "ctrl+end returns to bottom",
                editable: false,
            },
        ]
    }

    pub(super) fn kill_background_job(&mut self, id: &str) {
        let Some(job) = self.background_jobs.get(id) else {
            self.toast("Unknown background job", ToastKind::Error);
            self.status_line = "unknown job".to_string();
            return;
        };
        if job.state != ToolRunState::Running {
            self.toast("Job is not running", ToastKind::Warning);
            self.status_line = "job is not running".to_string();
            return;
        }
        let pid = job.pid;
        #[cfg(unix)]
        let status = Command::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .status();
        #[cfg(not(unix))]
        let status = Command::new("kill").arg(pid.to_string()).status();
        match status {
            Ok(status) if status.success() => {
                self.status_line = format!("kill sent · {id}");
                self.toast(self.status_line.clone(), ToastKind::Success);
            }
            Ok(status) => {
                self.status_line = format!("kill failed · exit {}", status.code().unwrap_or(-1));
                self.toast(self.status_line.clone(), ToastKind::Error);
            }
            Err(error) => {
                self.status_line = format!("kill failed: {error}");
                self.toast(self.status_line.clone(), ToastKind::Error);
            }
        }
    }

    pub(super) fn tail_background_job(&mut self, id: &str) {
        let Some(job) = self.background_jobs.get(id) else {
            self.toast("Unknown background job", ToastKind::Error);
            return;
        };
        let text = if job.last_output.trim().is_empty() {
            format!(
                "job {id}\npid: {}\ncommand: {}\noutput: <not available yet>",
                job.pid, job.command
            )
        } else {
            format!(
                "job {id}\npid: {}\ncommand: {}\n\n{}",
                job.pid, job.command, job.last_output
            )
        };
        self.transcript
            .push(TranscriptItem::Message(ChatMessage::system(text)));
        self.touch_transcript();
        self.status_line = format!("tailed job {id}");
    }

    pub(super) fn restart_background_job(&mut self, id: &str) {
        let Some(job) = self.background_jobs.get(id) else {
            self.toast("Unknown background job", ToastKind::Error);
            return;
        };
        let command = job.command.clone();
        self.start_exec_command(&command, true);
    }

    /// A runtime for tools the user invokes directly (/exec, /patch). The
    /// user typing the command IS the approval, so NeedsApproval auto-allows;
    /// hard denies still block. Uses an immediate closure (no channel) so it
    /// can run on the UI thread without deadlocking.
    pub(super) fn user_tools(&self) -> ToolRuntime {
        self.tools
            .clone()
            .with_approval_handler(Arc::new(|_request| ApprovalDecision::AllowOnce))
    }

    pub(super) fn start_exec_command(&mut self, command: &str, background: bool) {
        // A foreground /exec blocks the UI thread until the child exits, which
        // would also stall approval servicing for any running turn/workflow.
        if !background && (self.is_working() || self.has_active_workflows()) {
            self.status_line =
                "finish the current turn before running a foreground /exec".to_string();
            self.toast(
                "Busy — use /exec … & for background, or wait",
                ToastKind::Warning,
            );
            return;
        }
        self.push_tool_start("terminal.exec".to_string(), format!("$ {command}"));
        let request = TerminalExecRequest {
            command: command.to_string(),
            cwd: None,
            background,
            unsandboxed: false,
        };
        match self
            .user_tools()
            .with_background_events(self.background_job_sender.clone())
            .terminal_exec(request)
        {
            Ok(result) => {
                if result.background {
                    if let Some(id) = result.job_id.as_deref() {
                        self.attach_or_push_background_tool_start(id, command);
                        self.update_tool_result_by_id(
                            id,
                            ToolRunState::Running,
                            &terminal_result_output(&result),
                        );
                    } else {
                        self.push_tool_result("terminal.exec", terminal_result_output(&result));
                    }
                } else {
                    self.push_tool_result("terminal.exec", terminal_result_output(&result));
                }
                self.status_line = if result.background {
                    format!("terminal.exec background · pid {}", result.pid.unwrap_or(0))
                } else {
                    format!("terminal.exec exit {}", result.code.unwrap_or(-1))
                };
                self.toast(self.status_line.clone(), ToastKind::Success);
            }
            Err(error) => {
                self.push_tool_result("terminal.exec", format!("error: {error}"));
                self.status_line = "terminal.exec failed".to_string();
                self.toast("Command failed", ToastKind::Error);
            }
        }
    }
}
