use super::*;

pub(crate) fn tool_group_is_open(transcript: &[TranscriptItem], start: usize, end: usize) -> bool {
    transcript[start..end]
        .iter()
        .find_map(|item| match item {
            TranscriptItem::Tool(run) => Some(run.group_expanded),
            _ => None,
        })
        .unwrap_or(false)
}

/// The tool verb without the redundant leading name, e.g. "read src/main.rs" -> "src/main.rs".
pub(crate) fn tool_summary_rest(run: &ToolRun) -> String {
    let name = tool_display_name(&run.name);
    let summary = tool_summary(&run.summary);
    summary
        .strip_prefix(name)
        .map(str::trim_start)
        .unwrap_or(summary.as_str())
        .to_string()
}

/// Edits and patches carry diffs the user should see per-call; never merge them.
pub(crate) fn tool_name_coalescible(name: &str) -> bool {
    !matches!(name, "file.edit" | "file.patch")
}

/// True when the group contains at least one coalescible run — consecutive
/// succeeded calls to the same tool (reasoning items in between don't break a run).
pub(crate) fn tool_group_has_coalesced_runs(
    transcript: &[TranscriptItem],
    start: usize,
    end: usize,
) -> bool {
    let mut previous: Option<&str> = None;
    for item in &transcript[start..end] {
        match item {
            TranscriptItem::Tool(run)
                if run.state == ToolRunState::Succeeded && tool_name_coalescible(&run.name) =>
            {
                let name = tool_display_name(&run.name);
                if previous == Some(name) {
                    return true;
                }
                previous = Some(name);
            }
            TranscriptItem::Tool(_) => previous = None,
            _ => {}
        }
    }
    false
}

pub(crate) const TOOL_COALESCE_SHOWN_TARGETS: usize = 3;

pub(crate) fn append_coalesced_tool_lines(
    lines: &mut Vec<Line<'static>>,
    runs: &[&ToolRun],
    selected: bool,
    context: RenderContext,
) {
    let selection = if selected {
        activity_selected_style()
    } else {
        Style::default()
    };
    let sel = |style: Style| style.patch(selection);

    let name = tool_display_name(&runs[0].name);
    // A running call can only ever be the tail of a coalesced run.
    let active = runs
        .last()
        .filter(|run| run.state == ToolRunState::Running)
        .copied();
    let targets: Vec<String> = runs
        .iter()
        .filter(|run| run.state == ToolRunState::Succeeded)
        .map(|run| tool_summary_rest(run))
        .filter(|target| !target.is_empty())
        .collect();
    let extra = targets.len().saturating_sub(TOOL_COALESCE_SHOWN_TARGETS);
    let mut label = targets
        .iter()
        .take(TOOL_COALESCE_SHOWN_TARGETS)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if extra > 0 {
        label.push_str(&format!(" +{extra} more"));
    }
    if let Some(active) = active {
        let target = tool_summary_rest(active);
        if !target.is_empty() {
            if !label.is_empty() {
                label.push_str(", ");
            }
            label.push_str(&target);
        }
    }
    if label.is_empty() {
        label = format!("×{}", runs.len());
    }

    let marker = if active.is_some() {
        tool_running_marker_span(ToolRunState::Running, context.animation_tick)
    } else {
        Span::styled(TOOL_MARKER, sel(tool_marker_style()))
    };
    lines.push(Line::from(vec![
        marker,
        Span::raw(" "),
        Span::styled(
            name.to_string(),
            sel(tool_label_style().add_modifier(Modifier::BOLD)),
        ),
        Span::styled(
            format!(" {}", truncate(&label, 140)),
            sel(message_style(ChatRole::Tool)),
        ),
    ]));

    let mut result = vec![
        Span::raw("  "),
        Span::styled("⎿ ", separator_style()),
        Span::styled(format!("{} calls", runs.len()), muted()),
    ];
    if active.is_some() {
        result.push(Span::styled(" · running…", muted()));
    }
    if selected {
        result.push(Span::styled(" · enter to expand", muted()));
    }
    lines.push(Line::from(result));
}

pub(crate) fn append_tool_group_lines(
    lines: &mut Vec<Line<'static>>,
    transcript: &[TranscriptItem],
    start: usize,
    end: usize,
    selected_tool: Option<usize>,
    context: RenderContext,
) {
    let coalesce = !tool_group_is_open(transcript, start, end);
    let mut first = true;
    let mut index = start;
    while index < end {
        let TranscriptItem::Tool(run) = &transcript[index] else {
            index += 1;
            continue;
        };

        // Collect the consecutive run of succeeded calls to the same tool,
        // skipping reasoning items in between.
        let mut matched: Vec<&ToolRun> = vec![run];
        let mut cursor = index + 1;
        if coalesce
            && run.state == ToolRunState::Succeeded
            && !run.expanded
            && tool_name_coalescible(&run.name)
        {
            loop {
                let mut probe = cursor;
                while probe < end && matches!(transcript[probe], TranscriptItem::Reasoning(_)) {
                    probe += 1;
                }
                match transcript.get(probe) {
                    Some(TranscriptItem::Tool(next))
                        if probe < end
                            && next.state == ToolRunState::Succeeded
                            && !next.expanded
                            && tool_display_name(&next.name) == tool_display_name(&run.name) =>
                    {
                        matched.push(next);
                        cursor = probe + 1;
                    }
                    // A running call of the same tool joins as the live tail, so
                    // it doesn't render below only to jump into the run on success.
                    Some(TranscriptItem::Tool(next))
                        if probe < end
                            && next.state == ToolRunState::Running
                            && tool_display_name(&next.name) == tool_display_name(&run.name) =>
                    {
                        matched.push(next);
                        cursor = probe + 1;
                        break;
                    }
                    _ => break,
                }
            }
        }

        if !first {
            lines.push(Line::from(""));
        }
        first = false;

        if matched.len() > 1 {
            let selected = matches!(selected_tool, Some(sel) if sel >= index && sel < cursor);
            append_coalesced_tool_lines(lines, &matched, selected, context);
            index = cursor;
        } else {
            append_tool_call_lines(lines, run, selected_tool == Some(index), context);
            index += 1;
        }
    }
}

pub(crate) const TOOL_DETAIL_COLLAPSED_LINES: usize = 1;
pub(crate) const TOOL_DETAIL_FAILED_LINES: usize = 4;
pub(crate) const TOOL_DETAIL_EXPANDED_LINES: usize = 24;
/// Diffs are the payoff of an edit — show a real chunk of them by default.
pub(crate) const TOOL_DETAIL_DIFF_COLLAPSED_LINES: usize = 12;
pub(crate) const TOOL_DETAIL_DIFF_EXPANDED_LINES: usize = 64;

pub(crate) fn append_tool_call_lines(
    lines: &mut Vec<Line<'static>>,
    run: &ToolRun,
    selected: bool,
    context: RenderContext,
) {
    let selection = if selected {
        activity_selected_style()
    } else {
        Style::default()
    };
    let sel = |style: Style| style.patch(selection);

    let marker = match run.state {
        ToolRunState::Running => tool_running_marker_span(run.state, context.animation_tick),
        ToolRunState::Succeeded => Span::styled(TOOL_MARKER, sel(tool_marker_style())),
        ToolRunState::Failed => Span::styled(TOOL_MARKER, sel(error_style())),
    };

    let mut row = vec![marker, Span::raw(" ")];
    let name = tool_display_name(&run.name);
    // Summaries like "read AGENTS.md" already start with the tool verb, so
    // drop the redundant name to avoid "read read AGENTS.md".
    let summary = tool_summary(&run.summary);
    let summary_rest = summary
        .strip_prefix(name)
        .map(str::trim_start)
        .unwrap_or(summary.as_str());
    row.push(Span::styled(
        name.to_string(),
        sel(tool_label_style().add_modifier(Modifier::BOLD)),
    ));
    if !summary_rest.is_empty() {
        row.push(Span::styled(
            format!(" {}", truncate(summary_rest, 140)),
            sel(message_style(ChatRole::Tool)),
        ));
    }
    lines.push(Line::from(row));

    if run.state == ToolRunState::Running {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled("⎿ ", separator_style()),
            Span::styled("running…", muted()),
        ]));
        return;
    }

    let detail_lines = meaningful_tool_output_lines(run);
    if detail_lines.is_empty() {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled("⎿ ", separator_style()),
            match run.state {
                ToolRunState::Failed => Span::styled("failed", error_style()),
                _ => Span::styled("done", muted()),
            },
        ]));
        return;
    }

    let has_diff = tool_run_has_diff(run);
    let visible = if run.expanded {
        if has_diff {
            TOOL_DETAIL_DIFF_EXPANDED_LINES
        } else {
            TOOL_DETAIL_EXPANDED_LINES
        }
    } else if run.state == ToolRunState::Failed {
        TOOL_DETAIL_FAILED_LINES
    } else if has_diff {
        TOOL_DETAIL_DIFF_COLLAPSED_LINES
    } else {
        TOOL_DETAIL_COLLAPSED_LINES
    };
    let body_style = match run.state {
        ToolRunState::Failed => error_preview_style(),
        _ => muted(),
    };

    for (index, line) in detail_lines.iter().take(visible).enumerate() {
        let prefix = if index == 0 { "⎿ " } else { "  " };
        let mut row = vec![Span::raw("  "), Span::styled(prefix, separator_style())];
        if line.contains('\u{1b}') {
            row.extend(ansi_detail_spans(line, body_style));
        } else {
            let line_style = if has_diff && run.state != ToolRunState::Failed {
                diff_line_style(line).unwrap_or(body_style)
            } else {
                body_style
            };
            row.push(Span::styled(truncate(line, 170), line_style));
        }
        lines.push(Line::from(row));
    }

    let hidden = detail_lines.len().saturating_sub(visible);
    if hidden > 0 {
        let hint = if run.expanded {
            format!(
                "… +{hidden} more line{}",
                if hidden == 1 { "" } else { "s" }
            )
        } else {
            format!(
                "… +{hidden} line{} (enter to expand)",
                if hidden == 1 { "" } else { "s" }
            )
        };
        lines.push(Line::from(vec![
            Span::raw("    "),
            Span::styled(hint, muted()),
        ]));
    }
}

pub(crate) fn tool_running_marker_span(state: ToolRunState, animation_tick: u64) -> Span<'static> {
    match state {
        ToolRunState::Running => {
            let frame = animation::ThrobberKind::ToolPulse.frame(animation_tick);
            Span::styled(frame.symbol, tool_pulse_style(frame))
        }
        ToolRunState::Succeeded => Span::styled("✓", success_style()),
        ToolRunState::Failed => Span::styled("×", error_style()),
    }
}

pub(crate) fn tool_pulse_style(frame: animation::ThrobberFrame) -> Style {
    match frame.energy {
        3 => tool_label_style().add_modifier(Modifier::BOLD),
        2 => Style::default()
            .fg(accent_color())
            .add_modifier(Modifier::BOLD),
        1 => Style::default()
            .fg(accent_color())
            .add_modifier(Modifier::BOLD),
        _ => muted(),
    }
}

pub(crate) fn light_sweep_spans(
    text: &str,
    animation_tick: u64,
    style_patch: impl Fn(Style) -> Style,
) -> Vec<Span<'static>> {
    let chars = text.chars().collect::<Vec<_>>();
    if chars.is_empty() {
        return Vec::new();
    }

    let char_count = chars.len();
    chars
        .into_iter()
        .enumerate()
        .map(|(index, ch)| {
            let distance =
                animation::light_sweep_distance(index, char_count, animation_tick).unwrap_or(0);
            let style = match distance {
                0 => style_patch(tool_label_style().add_modifier(Modifier::BOLD)),
                1..=2 => style_patch(Style::default().fg(accent_color())),
                3..=4 => style_patch(message_style(ChatRole::Tool)),
                _ => style_patch(tool_group_meta_style()),
            };
            Span::styled(ch.to_string(), style)
        })
        .collect()
}

pub(crate) fn tool_display_name(name: &str) -> &str {
    match name {
        "file.read" => "read",
        "file.search" => "search",
        "semantic.search" => "semantic",
        "fs.list" => "list",
        "terminal.exec" => "terminal",
        "file.edit" => "edit",
        "file.patch" => "patch",
        "web.fetch" => "fetch",
        "web.search" => "web",
        "task.update" => "status",
        other => other,
    }
}

pub(crate) fn meaningful_tool_output_lines(run: &ToolRun) -> Vec<String> {
    run.detail
        .lines()
        // trim_end only: leading whitespace is meaningful in diff output.
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty() && line.trim() != "done")
        .map(ToString::to_string)
        .collect()
}

/// True when this run's detail carries a display diff from file.edit/file.patch.
pub(crate) fn tool_run_has_diff(run: &ToolRun) -> bool {
    matches!(run.name.as_str(), "file.edit" | "file.patch")
}

/// Convert a detail line containing ANSI escape codes into styled spans.
/// Unstyled segments fall back to the tool body style so colored fragments
/// (e.g. cargo's red `error`) sit inside otherwise-muted output.
pub(crate) fn ansi_detail_spans(line: &str, fallback: Style) -> Vec<Span<'static>> {
    use ansi_to_tui::IntoText;

    let Ok(text) = line.into_text() else {
        return vec![Span::styled(line.replace('\u{1b}', "␛"), fallback)];
    };
    let Some(parsed) = text.lines.into_iter().next() else {
        return Vec::new();
    };
    parsed
        .spans
        .into_iter()
        .map(|span| {
            // Reset/uncolored segments take the tool body style; ansi-to-tui
            // encodes SGR reset as explicit Color::Reset rather than default.
            let unstyled = match span.style.fg {
                None | Some(Color::Reset) => true,
                Some(_) => false,
            };
            let style = if unstyled { fallback } else { span.style };
            Span::styled(span.content.into_owned(), style)
        })
        .collect()
}

pub(crate) fn diff_line_style(line: &str) -> Option<Style> {
    let trimmed = line.trim_start();
    if trimmed.contains("verify:") && trimmed.contains("FAILED") {
        return Some(error_style());
    }
    if trimmed.starts_with("+") {
        Some(Style::default().fg(palette().success))
    } else if trimmed.starts_with("-") {
        Some(Style::default().fg(palette().error))
    } else if trimmed.starts_with("@@") {
        Some(muted().add_modifier(Modifier::DIM))
    } else {
        None
    }
}

pub(crate) fn tool_output_failed(output: &str) -> bool {
    if output.starts_with("failed") || output.starts_with("error:") {
        return true;
    }

    if let Some(exit) = output.strip_prefix("exit: ") {
        let code = exit.split_whitespace().next().unwrap_or("");
        return code != "0";
    }

    false
}

pub(crate) fn compact_tool_detail(output: &str) -> String {
    if output.trim().is_empty() {
        return "done".to_string();
    }

    let is_raw_terminal = output.starts_with("exit:")
        && output
            .lines()
            .any(|line| matches!(line.trim(), "stdout:" | "stderr:" | "stdout: <empty>"));

    // Edit/patch diffs and pre-summarized terminal output are already compacted
    // upstream and their body is the whole point of the expanded view; keep them whole.
    if !is_raw_terminal
        && (output.starts_with("edited ")
            || output.starts_with("patched ")
            || output.starts_with("exit:"))
    {
        return output.to_string();
    }

    // Raw terminal-format output (background streams): drop section markers.
    if is_raw_terminal
        || output.starts_with("patched files:")
        || output.starts_with("edited files:")
    {
        return output
            .lines()
            .filter(|line| {
                let line = line.trim();
                !line.is_empty()
                    && line != "stdout:"
                    && line != "stderr:"
                    && line != "stdout: <empty>"
            })
            .skip(1)
            .take(40)
            .collect::<Vec<_>>()
            .join("\n")
            .if_empty("done");
    }

    output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .take(40)
        .collect::<Vec<_>>()
        .join("\n")
}
