use super::*;

impl App {
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
            let hit_rate = usage
                .cache_hit_percent()
                .map(|percent| format!("{percent:.1}%"))
                .unwrap_or_else(|| "n/a".to_string());
            Line::from(Span::styled(
                format!(
                    "  input {} · output {} · cache hit {} · cached {} · uncached {}",
                    format_token_count(usage.input),
                    format_token_count(usage.output),
                    hit_rate,
                    format_token_count(usage.cached),
                    format_token_count(usage.uncached_input()),
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
        let image_input_warning = image_input_warning(self.model.model_capabilities().images);
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
}
