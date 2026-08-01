use super::*;

impl App {
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
                Span::styled(self.model.model_name().to_string(), prompt_style()),
                Span::styled(" · ", muted()),
                Span::styled(self.model.reasoning_effort().to_string(), prompt_style()),
            ]),
            Line::from(vec![
                Span::styled("choose effort or Ultra orchestration", muted()),
                Span::styled("  ·  ", muted()),
                Span::styled("/model <provider/model>", prompt_style()),
                Span::styled(" accepts configured models", muted()),
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
}
