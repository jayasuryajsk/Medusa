use super::*;

impl App {
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
}
