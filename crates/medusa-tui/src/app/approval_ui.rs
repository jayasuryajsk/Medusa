use super::*;

#[derive(Clone, Copy)]
struct ApprovalOption {
    decision: ApprovalDecision,
    shortcut: char,
    label: &'static str,
}

impl App {
    pub(super) fn reset_approval_ui(&mut self) {
        self.approval_shown_at = None;
        self.approval_selection = 0;
        self.approval_expanded = false;
        self.approval_detail_scroll = 0;
    }

    pub(super) fn handle_approval_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.should_quit = true;
            return;
        }
        if key.code == KeyCode::Char('a') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.approval_expanded = !self.approval_expanded;
            self.approval_detail_scroll = 0;
            self.status_line = if self.approval_expanded {
                "approval details expanded"
            } else {
                "approval details collapsed"
            }
            .to_string();
            return;
        }

        // Consume decision keys briefly after the prompt appears so the key
        // that triggered a tool action cannot also approve it accidentally.
        if self
            .approval_shown_at
            .is_none_or(|shown| shown.elapsed() < APPROVAL_KEY_GRACE)
        {
            if self.approval_shown_at.is_none() {
                self.approval_shown_at = Some(Instant::now());
            }
            return;
        }

        let Some(request) = self
            .approval_queue
            .front()
            .map(|pending| pending.request.clone())
        else {
            return;
        };
        let options = approval_options(&request);
        if options.is_empty() {
            return;
        }
        self.approval_selection = self.approval_selection.min(options.len() - 1);

        let plain = key.modifiers.difference(KeyModifiers::SHIFT).is_empty();
        if !plain {
            return;
        }
        match key.code {
            KeyCode::Up | KeyCode::BackTab => {
                self.approval_selection = self
                    .approval_selection
                    .checked_sub(1)
                    .unwrap_or(options.len() - 1);
            }
            KeyCode::Down | KeyCode::Tab => {
                self.approval_selection = (self.approval_selection + 1) % options.len();
            }
            KeyCode::Home => self.approval_selection = 0,
            KeyCode::End => self.approval_selection = options.len() - 1,
            KeyCode::PageUp if self.approval_expanded => {
                self.approval_detail_scroll = self.approval_detail_scroll.saturating_sub(5);
            }
            KeyCode::PageDown if self.approval_expanded => {
                self.approval_detail_scroll = self.approval_detail_scroll.saturating_add(5);
            }
            KeyCode::Enter => {
                self.resolve_pending_approval(options[self.approval_selection].decision);
            }
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                self.resolve_pending_approval(ApprovalDecision::AllowOnce);
            }
            KeyCode::Char('a') | KeyCode::Char('A') => {
                if options
                    .iter()
                    .any(|option| option.decision == ApprovalDecision::AlwaysAllow)
                {
                    self.resolve_pending_approval(ApprovalDecision::AlwaysAllow);
                }
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.last_escape_at = None;
                self.resolve_pending_approval(ApprovalDecision::Deny);
            }
            KeyCode::Char(number) if number.is_ascii_digit() => {
                if let Some(index) = number.to_digit(10).and_then(|n| n.checked_sub(1)) {
                    let index = index as usize;
                    if let Some(option) = options.get(index) {
                        self.resolve_pending_approval(option.decision);
                    }
                }
            }
            _ => {}
        }
    }

    pub(super) fn approval_pane_height(&self, width: u16, terminal_height: u16) -> u16 {
        if self.approval_expanded {
            return terminal_height.saturating_sub(3).max(6);
        }
        let Some(request) = self.approval_queue.front().map(|pending| &pending.request) else {
            return 0;
        };
        let content_width = width.saturating_sub(4).max(1) as usize;
        let detail_height = approval_detail_lines(request, content_width)
            .len()
            .clamp(1, 6) as u16;
        let option_height = approval_options(request).len() as u16;
        let desired = detail_height
            .saturating_add(option_height)
            .saturating_add(5);
        let available = terminal_height.saturating_sub(8).max(8);
        desired.clamp(8, available)
    }

    pub(super) fn draw_approval_pane(&self, frame: &mut Frame<'_>, area: Rect) {
        let Some(request) = self.approval_queue.front().map(|pending| &pending.request) else {
            return;
        };
        if area.width == 0 || area.height == 0 {
            return;
        }

        frame.render_widget(Block::default().style(Style::default().bg(surface())), area);
        let inner = area.inner(Margin {
            horizontal: 2.min(area.width / 2),
            vertical: 0,
        });
        let options = approval_options(request);
        let option_height = options.len() as u16;
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(1),
                Constraint::Length(1),
                Constraint::Length(option_height),
                Constraint::Length(1),
            ])
            .split(inner);

        let queue_suffix = if self.approval_queue.len() > 1 {
            format!("  1 of {}", self.approval_queue.len())
        } else {
            String::new()
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    approval_question(request),
                    value_style().add_modifier(Modifier::BOLD),
                ),
                Span::styled(queue_suffix, muted()),
            ]))
            .style(Style::default().bg(surface()).fg(text())),
            sections[0],
        );

        let detail_lines = approval_detail_lines(request, sections[2].width.max(1) as usize);
        let max_scroll = detail_lines
            .len()
            .saturating_sub(sections[2].height as usize) as u16;
        let detail_scroll = self.approval_detail_scroll.min(max_scroll);
        frame.render_widget(
            Paragraph::new(detail_lines)
                .style(Style::default().bg(surface()).fg(text()))
                .scroll((detail_scroll, 0)),
            sections[2],
        );

        let rows = options
            .iter()
            .enumerate()
            .map(|(index, option)| {
                let selected = index == self.approval_selection.min(options.len() - 1);
                let marker = if selected { "› " } else { "  " };
                let row_style = if selected {
                    accent().add_modifier(Modifier::BOLD)
                } else {
                    value_style()
                };
                Line::from(vec![
                    Span::styled(marker, if selected { accent() } else { muted() }),
                    Span::styled(format!("{}. {}", index + 1, option.label), row_style),
                    Span::styled(format!(" ({})", option.shortcut), muted()),
                ])
            })
            .collect::<Vec<_>>();
        frame.render_widget(
            Paragraph::new(rows).style(Style::default().bg(surface()).fg(text())),
            sections[4],
        );

        let detail_hint = if self.approval_expanded {
            "pgup/pgdn scroll · ctrl+a collapse"
        } else {
            "ctrl+a details"
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("↑/↓", prompt_style()),
                Span::styled(" select  ", muted()),
                Span::styled("enter", prompt_style()),
                Span::styled(" confirm  ", muted()),
                Span::styled("esc", prompt_style()),
                Span::styled(" deny  ·  ", muted()),
                Span::styled(detail_hint, muted()),
            ]))
            .style(Style::default().bg(surface()).fg(text())),
            sections[5],
        );
    }
}

fn approval_options(request: &ApprovalRequest) -> Vec<ApprovalOption> {
    let mut options = vec![ApprovalOption {
        decision: ApprovalDecision::AllowOnce,
        shortcut: 'y',
        label: allow_once_label(request),
    }];
    if !request.sandbox_escalation {
        options.push(ApprovalOption {
            decision: ApprovalDecision::AlwaysAllow,
            shortcut: 'a',
            label: always_allow_label(request),
        });
    }
    options.push(ApprovalOption {
        decision: ApprovalDecision::Deny,
        shortcut: 'n',
        label: deny_label(request),
    });
    options
}

fn approval_question(request: &ApprovalRequest) -> &'static str {
    if request.sandbox_escalation {
        return "Would you like to run this command outside the sandbox?";
    }
    match request.tool {
        ApprovalTool::TerminalExec => "Would you like to run this command?",
        ApprovalTool::FileEdit | ApprovalTool::FilePatch => "Would you like to make these edits?",
        ApprovalTool::McpTool => "Would you like to call this MCP tool?",
        ApprovalTool::McpServerLaunch => "Would you like to start this MCP server?",
        ApprovalTool::WebFetch => "Would you like to fetch this URL?",
        ApprovalTool::WebSearch => "Would you like to search the web?",
    }
}

fn allow_once_label(request: &ApprovalRequest) -> &'static str {
    match request.tool {
        ApprovalTool::TerminalExec => "Yes, proceed",
        ApprovalTool::FileEdit | ApprovalTool::FilePatch => "Yes, make these edits",
        ApprovalTool::McpTool => "Yes, call this tool",
        ApprovalTool::McpServerLaunch => "Yes, start this server",
        ApprovalTool::WebFetch => "Yes, fetch this URL",
        ApprovalTool::WebSearch => "Yes, run this search",
    }
}

fn always_allow_label(request: &ApprovalRequest) -> &'static str {
    match request.tool {
        ApprovalTool::TerminalExec => "Yes, and remember matching commands",
        ApprovalTool::FileEdit | ApprovalTool::FilePatch => "Yes, and remember these paths",
        ApprovalTool::McpTool => "Yes, and remember this tool",
        ApprovalTool::McpServerLaunch => "Yes, and remember this server",
        ApprovalTool::WebFetch | ApprovalTool::WebSearch => {
            "Yes, allow web access for this session"
        }
    }
}

fn deny_label(request: &ApprovalRequest) -> &'static str {
    match request.tool {
        ApprovalTool::FileEdit | ApprovalTool::FilePatch => "No, continue without editing",
        _ => "No, continue without running it",
    }
}

fn approval_detail_lines(request: &ApprovalRequest, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut lines = Vec::new();
    lines.push(Line::from(vec![
        Span::styled(request.tool.label(), tool_label_style()),
        Span::styled(
            if request.background {
                "  background"
            } else {
                ""
            },
            muted(),
        ),
    ]));

    if request.sandbox_escalation {
        lines.push(Line::from(Span::styled(
            "This can write outside the workspace and use unrestricted network access.",
            error_style(),
        )));
    }

    if let Some(command) = request
        .command
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let label = match request.tool {
            ApprovalTool::TerminalExec => "$ ",
            ApprovalTool::WebFetch => "URL  ",
            ApprovalTool::WebSearch => "query  ",
            ApprovalTool::McpTool | ApprovalTool::McpServerLaunch => "request  ",
            ApprovalTool::FileEdit | ApprovalTool::FilePatch => "",
        };
        let continuation = " ".repeat(label.len());
        let command_width = width.saturating_sub(label.len()).max(1);
        for (index, chunk) in wrap_str(command, command_width).into_iter().enumerate() {
            lines.push(Line::from(vec![
                Span::styled(
                    if index == 0 {
                        label.to_string()
                    } else {
                        continuation.clone()
                    },
                    prompt_style(),
                ),
                Span::styled(chunk, value_style()),
            ]));
        }
    }

    for path in &request.paths {
        let path_width = width.saturating_sub(3).max(1);
        for (index, chunk) in wrap_str(path, path_width).into_iter().enumerate() {
            lines.push(Line::from(vec![
                Span::styled(if index == 0 { "→ " } else { "  " }, separator_style()),
                Span::styled(chunk, value_style()),
            ]));
        }
    }
    lines
}
