use medusa_core::permissions::PermissionMode;
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders},
};

use crate::config::{ThemeKind, model_display, palette, reasoning_description};
use crate::types::{ChatRole, ToastKind, ToolRunState};

pub(crate) fn app_bg() -> Color {
    Color::Reset
}

pub(crate) fn surface() -> Color {
    Color::Reset
}

pub(crate) fn text() -> Color {
    palette().text
}

pub(crate) fn accent_color() -> Color {
    palette().accent
}

pub(crate) fn muted() -> Style {
    Style::default().fg(palette().muted)
}

pub(crate) fn accent() -> Style {
    Style::default().fg(accent_color())
}

pub(crate) fn value_style() -> Style {
    Style::default().fg(text())
}

pub(crate) fn separator_style() -> Style {
    Style::default().fg(palette().separator)
}

pub(crate) fn prompt_style() -> Style {
    Style::default()
        .fg(palette().prompt)
        .add_modifier(Modifier::BOLD)
}

pub(crate) fn attachment_preview_border_style() -> Style {
    separator_style()
}

pub(crate) fn attachment_preview_title_style() -> Style {
    success_style()
}

pub(crate) fn attachment_preview_meta_style() -> Style {
    muted()
}

pub(crate) fn tool_label_style() -> Style {
    Style::default()
        .fg(palette().tool)
        .add_modifier(Modifier::BOLD)
}

/// Marker for finished tool calls — small bullet in the theme's tool accent
/// (failures override with the error color).
pub(crate) const TOOL_MARKER: &str = "•";

pub(crate) fn tool_marker_style() -> Style {
    Style::default()
        .fg(palette().tool)
        .add_modifier(Modifier::BOLD)
}

pub(crate) fn tool_group_label_style() -> Style {
    Style::default()
        .fg(palette().tool)
        .add_modifier(Modifier::BOLD)
}

pub(crate) fn tool_group_meta_style() -> Style {
    muted()
}

pub(crate) fn success_style() -> Style {
    Style::default()
        .fg(palette().success)
        .add_modifier(Modifier::BOLD)
}

pub(crate) fn error_style() -> Style {
    Style::default()
        .fg(palette().error)
        .add_modifier(Modifier::BOLD)
}

pub(crate) fn error_preview_style() -> Style {
    Style::default().fg(palette().error)
}

pub(crate) fn tool_output_style(state: ToolRunState) -> Style {
    match state {
        ToolRunState::Failed => error_preview_style(),
        _ => message_style(ChatRole::Tool),
    }
}

pub(crate) fn code_border_style() -> Style {
    separator_style()
}

pub(crate) fn code_block_style() -> Style {
    Style::default().fg(palette().code_fg).bg(palette().code_bg)
}

pub(crate) fn inline_code_style() -> Style {
    Style::default()
        .fg(palette().inline_code_fg)
        .bg(palette().inline_code_bg)
}

pub(crate) fn heading_style(level: usize) -> Style {
    let color = if level <= 2 {
        accent_color()
    } else {
        palette().info
    };

    Style::default().fg(color).add_modifier(Modifier::BOLD)
}

pub(crate) fn quote_border_style() -> Style {
    Style::default().fg(palette().quote)
}

pub(crate) fn quote_style() -> Style {
    Style::default()
        .fg(palette().quote)
        .add_modifier(Modifier::ITALIC)
}

pub(crate) fn list_marker_style() -> Style {
    Style::default()
        .fg(palette().prompt)
        .add_modifier(Modifier::BOLD)
}

pub(crate) fn link_style() -> Style {
    Style::default()
        .fg(palette().info)
        .add_modifier(Modifier::UNDERLINED)
}

pub(crate) fn user_message_background_style() -> Style {
    Style::default().bg(palette().user_bg)
}

pub(crate) fn user_message_style() -> Style {
    user_message_background_style().fg(palette().text)
}

pub(crate) fn user_message_prompt_style() -> Style {
    user_message_background_style()
        .fg(palette().prompt)
        .add_modifier(Modifier::BOLD)
}

pub(crate) fn message_style(role: ChatRole) -> Style {
    match role {
        ChatRole::User => user_message_style(),
        ChatRole::Assistant => value_style(),
        ChatRole::Tool => Style::default().fg(palette().tool),
        ChatRole::System => error_preview_style(),
    }
}

pub(crate) fn command_selected_style() -> Style {
    Style::default()
        .fg(palette().selected_fg)
        .bg(palette().selected_bg)
        .add_modifier(Modifier::BOLD)
}

pub(crate) fn activity_selected_style() -> Style {
    Style::default().bg(palette().activity_bg)
}

pub(crate) fn toast_style(kind: ToastKind) -> Style {
    match kind {
        ToastKind::Info => Style::default().fg(palette().info),
        ToastKind::Success => success_style(),
        ToastKind::Warning => prompt_style(),
        ToastKind::Error => error_style(),
    }
}

pub(crate) fn toast_label(kind: ToastKind) -> &'static str {
    match kind {
        ToastKind::Info => "notice",
        ToastKind::Success => "done",
        ToastKind::Warning => "warning",
        ToastKind::Error => "error",
    }
}

pub(crate) fn modal_block(title: &'static str) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(accent_color()))
        .style(Style::default().bg(surface()).fg(text()))
}

pub(crate) fn centered_rect(parent: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(parent.width);
    let height = height.min(parent.height);
    let x = parent.x + parent.width.saturating_sub(width) / 2;
    let y = parent.y + parent.height.saturating_sub(height) / 2;

    Rect::new(x, y, width, height)
}

pub(crate) fn command_palette_rect(parent: Rect, item_count: usize) -> Rect {
    let max_width = parent.width.saturating_sub(4).max(1);
    let width = max_width.min(92);
    let max_height = parent.height.saturating_sub(4).max(3);
    let desired_height = (item_count as u16 + 5).clamp(9, 18);
    let height = desired_height.min(max_height);

    centered_rect(parent, width, height)
}

pub(crate) fn cursor_style() -> Style {
    Style::default().fg(accent_color())
}

pub(crate) fn placeholder_style() -> Style {
    muted().add_modifier(Modifier::ITALIC)
}

pub(crate) fn theme_preview_swatch_line(
    label: &'static str,
    color: Color,
    sample: &'static str,
) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<8}"), muted()),
        Span::styled("███", Style::default().fg(color)),
        Span::styled("  ", muted()),
        Span::styled(
            sample,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
    ])
}

pub(crate) fn theme_preview_lines(theme: ThemeKind) -> Vec<Line<'static>> {
    let palette = theme.palette();
    vec![
        Line::from(vec![
            Span::styled("swatches ", muted()),
            Span::styled("██", Style::default().fg(palette.accent)),
            Span::styled(" ", muted()),
            Span::styled("██", Style::default().fg(palette.prompt)),
            Span::styled(" ", muted()),
            Span::styled("██", Style::default().fg(palette.tool)),
            Span::styled(" ", muted()),
            Span::styled("██", Style::default().fg(palette.success)),
            Span::styled(" ", muted()),
            Span::styled("██", Style::default().fg(palette.error)),
        ]),
        theme_preview_swatch_line("accent", palette.accent, "selection / focus"),
        theme_preview_swatch_line("prompt", palette.prompt, "typed prompts"),
        theme_preview_swatch_line("tool", palette.tool, "tool calls"),
        theme_preview_swatch_line("success", palette.success, "completed work"),
        theme_preview_swatch_line("error", palette.error, "failures"),
        Line::from(vec![
            Span::styled("sample  ", muted()),
            Span::styled(
                "› ask Medusa to inspect code",
                Style::default().fg(palette.prompt),
            ),
        ]),
        Line::from(vec![
            Span::styled("answer  ", muted()),
            Span::styled(
                "Markdown, tools, and code render with this palette.",
                Style::default().fg(palette.text),
            ),
        ]),
        Line::from(vec![
            Span::styled("user    ", muted()),
            Span::styled(
                " message surface ",
                Style::default().fg(palette.text).bg(palette.user_bg),
            ),
            Span::styled("  ", muted()),
            Span::styled(
                "inline code",
                Style::default()
                    .fg(palette.inline_code_fg)
                    .bg(palette.inline_code_bg),
            ),
        ]),
    ]
}

pub(crate) fn model_picker_detail_lines(
    selected_model: &str,
    selected_effort: &str,
    active_model: &str,
    active_effort: &str,
) -> Vec<Line<'static>> {
    let is_active = selected_model == active_model && selected_effort == active_effort;
    let (display_name, description) = model_display(selected_model);
    let mut lines = vec![Line::from(vec![
        Span::styled(
            display_name.clone(),
            prompt_style().add_modifier(Modifier::BOLD),
        ),
        Span::styled("  ", muted()),
        Span::styled(
            if is_active { "active" } else { "ready to save" },
            if is_active { success_style() } else { muted() },
        ),
    ])];
    if display_name != selected_model {
        lines.push(Line::from(Span::styled(
            selected_model.to_string(),
            muted(),
        )));
    }
    lines.push(Line::from(""));
    if let Some(description) = description {
        lines.push(Line::from(Span::styled(description, value_style())));
        lines.push(Line::from(""));
    }
    lines.push(Line::from(vec![
        Span::styled(
            if selected_effort.eq_ignore_ascii_case("ultra") {
                "mode  "
            } else {
                "reasoning  "
            },
            muted(),
        ),
        Span::styled(
            selected_effort.to_string(),
            prompt_style().add_modifier(Modifier::BOLD),
        ),
    ]));
    if let Some(description) = reasoning_description(selected_model, selected_effort) {
        lines.push(Line::from(Span::styled(description, value_style())));
    }
    if selected_effort.eq_ignore_ascii_case("ultra") {
        lines.push(Line::from(vec![
            Span::styled("model effort  ", muted()),
            Span::styled("max", value_style()),
        ]));
        lines.push(Line::from(vec![
            Span::styled("orchestration  ", muted()),
            Span::styled("proactive workflows + subagents", value_style()),
        ]));
    }
    lines.extend([
        Line::from(vec![
            Span::styled("backend  ", muted()),
            Span::styled(model_backend_hint(selected_model), value_style()),
        ]),
        Line::from(vec![
            Span::styled("config  ", muted()),
            Span::styled(".medusa/settings.json", value_style()),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            if selected_effort.eq_ignore_ascii_case("ultra") {
                "Ultra is orchestrated by Medusa; it is never sent as a raw reasoning value."
            } else {
                "Applies to new turns. Active work keeps its current model and effort."
            },
            muted(),
        )),
    ]);
    lines
}

pub(crate) fn reasoning_detail_lines(
    model: &str,
    selected: &str,
    active: &str,
) -> Vec<Line<'static>> {
    let is_active = selected == active;
    let mut lines = vec![Line::from(vec![
        Span::styled(
            selected.to_string(),
            prompt_style().add_modifier(Modifier::BOLD),
        ),
        Span::styled("  ", muted()),
        Span::styled(
            if is_active { "active" } else { "ready to save" },
            if is_active { success_style() } else { muted() },
        ),
    ])];
    lines.push(Line::from(""));
    // Backend-provided description for this effort, when the model cache has one.
    if let Some(description) = reasoning_description(model, selected) {
        lines.push(Line::from(Span::styled(description, value_style())));
        lines.push(Line::from(""));
    }
    if selected.eq_ignore_ascii_case("ultra") {
        lines.extend([
            Line::from(vec![
                Span::styled("model effort  ", muted()),
                Span::styled("max", value_style()),
            ]),
            Line::from(vec![
                Span::styled("orchestration  ", muted()),
                Span::styled("proactive workflows + subagents", value_style()),
            ]),
            Line::from(""),
        ]);
    }
    lines.extend([
        Line::from(Span::styled(
            if selected.eq_ignore_ascii_case("ultra") {
                "Ultra is a Medusa orchestration mode, not a raw API reasoning value."
            } else {
                "Applies to new turns; active streams keep the effort they started with."
            },
            value_style(),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("override  ", muted()),
            Span::styled("MEDUSA_REASONING_EFFORT", value_style()),
            Span::styled(" wins for one-off launches", muted()),
        ]),
        Line::from(vec![
            Span::styled("none  ", muted()),
            Span::styled("disables reasoning entirely", value_style()),
        ]),
    ]);
    lines
}

pub(crate) fn model_backend_hint(model: &str) -> String {
    let provider = model.split_once('/').map(|(provider, _)| provider);
    match provider {
        Some("codex") => "Codex provider · uses Codex OAuth".to_string(),
        Some("deepseek") => "DeepSeek provider · API key required".to_string(),
        Some("openai") => "OpenAI provider · API key required".to_string(),
        Some("ollama" | "lmstudio") => "Local provider · no credentials required".to_string(),
        Some(provider) => format!("{provider} provider · see /auth for status"),
        None => "legacy model id · resolved through the active provider".to_string(),
    }
}

pub(crate) fn permission_detail_lines(
    selected: PermissionMode,
    active: PermissionMode,
) -> Vec<Line<'static>> {
    let is_active = selected == active;
    let mut lines = vec![
        Line::from(vec![
            Span::styled(selected.label(), prompt_style()),
            Span::styled("  ", muted()),
            Span::styled(
                if is_active { "active" } else { "ready to save" },
                if is_active { success_style() } else { muted() },
            ),
        ]),
        Line::from(""),
        Line::from(Span::styled(selected.description(), value_style())),
        Line::from(""),
    ];

    match selected {
        PermissionMode::Open => {
            lines.extend([
                Line::from(vec![
                    Span::styled("terminal  ", muted()),
                    Span::styled(
                        "allow unless explicitly denied by future custom config",
                        value_style(),
                    ),
                ]),
                Line::from(vec![
                    Span::styled("edits     ", muted()),
                    Span::styled("allow within workspace boundary", value_style()),
                ]),
            ]);
        }
        PermissionMode::Ask => {
            lines.extend([
                Line::from(vec![
                    Span::styled("terminal  ", muted()),
                    Span::styled(
                        "safe reads run freely; other commands pause for approval",
                        value_style(),
                    ),
                ]),
                Line::from(vec![
                    Span::styled("edits     ", muted()),
                    Span::styled("file edits and patches pause for approval", value_style()),
                ]),
                Line::from(vec![
                    Span::styled("grants    ", muted()),
                    Span::styled(
                        "'always allow' persists to .medusa/permissions.json",
                        value_style(),
                    ),
                ]),
            ]);
        }
        PermissionMode::Guarded => {
            lines.extend([
                Line::from(vec![
                    Span::styled("terminal  ", muted()),
                    Span::styled("deny common destructive fragments", value_style()),
                ]),
                Line::from(vec![
                    Span::styled("edits     ", muted()),
                    Span::styled("deny .git and Medusa session internals", value_style()),
                ]),
            ]);
        }
        PermissionMode::Readonly => {
            lines.extend([
                Line::from(vec![
                    Span::styled("terminal  ", muted()),
                    Span::styled("allow common inspection commands", value_style()),
                ]),
                Line::from(vec![
                    Span::styled("edits     ", muted()),
                    Span::styled("block file_edit and file_patch", value_style()),
                ]),
            ]);
        }
    }

    lines.extend([
        Line::from(""),
        Line::from(vec![
            Span::styled("config  ", muted()),
            Span::styled(".medusa/permissions.json", value_style()),
        ]),
    ]);
    lines
}
