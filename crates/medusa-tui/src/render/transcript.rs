use super::*;

#[cfg(test)]
pub(crate) fn visible_transcript_lines(
    transcript: &[TranscriptItem],
    streaming_message: Option<usize>,
    selected_tool: Option<usize>,
) -> Vec<Line<'static>> {
    transcript_lines_from_rows(&visible_transcript_rows(
        transcript,
        streaming_message,
        selected_tool,
        RenderContext::static_view(),
    ))
}

pub(crate) fn visible_transcript_rows(
    transcript: &[TranscriptItem],
    streaming_message: Option<usize>,
    selected_tool: Option<usize>,
    context: RenderContext,
) -> Vec<TranscriptRow> {
    if transcript.is_empty() {
        return launch_rows();
    }

    let mut rows = Vec::new();
    if should_preserve_launch_rows(transcript, streaming_message) {
        rows.extend(launch_rows());
        rows.push(TranscriptRow::text(Line::from("")));
    }
    let mut index = 0;
    while index < transcript.len() {
        match &transcript[index] {
            TranscriptItem::Message(message) if message.role == ChatRole::Assistant => {
                let (turn_start, turn_end) = activity_turn_range(transcript, index);
                // Earlier assistant text in this user turn is operational
                // narration. Keep it in history, but render only the latest
                // answer candidate after the turn-level activity batch.
                let has_activity = turn_has_activity(transcript, turn_start, turn_end);
                if !has_activity
                    || final_assistant_index(transcript, turn_start, turn_end) == Some(index)
                {
                    if has_activity {
                        append_turn_activity_rows(
                            &mut rows,
                            transcript,
                            turn_start,
                            turn_end,
                            selected_tool,
                            context,
                        );
                    }
                    append_chat_message_rows(&mut rows, message, streaming_message == Some(index));
                }
                index += 1;
            }
            TranscriptItem::Message(message) => {
                let is_streaming = streaming_message == Some(index);
                if !rows.is_empty() && message.role == ChatRole::User {
                    rows.push(TranscriptRow::text(Line::from("")));
                }
                append_chat_message_rows(&mut rows, message, is_streaming);
                index += 1;
                if message.role == ChatRole::User {
                    rows.push(TranscriptRow::text(Line::from("")));
                }
            }
            TranscriptItem::Workflow(workflow) => {
                if !rows.is_empty() {
                    rows.push(TranscriptRow::text(Line::from("")));
                }
                append_workflow_rows(&mut rows, workflow, context);
                index += 1;
            }
            // Plans render in the live strip above the composer, not in the
            // transcript; the item stays only as state (persistence + strip).
            TranscriptItem::Plan(_) => {
                index += 1;
            }
            TranscriptItem::Decision(decision) => {
                if !rows.is_empty() {
                    rows.push(TranscriptRow::text(Line::from("")));
                }
                append_decision_rows(&mut rows, decision, context.decision_selection);
                index += 1;
            }
            TranscriptItem::Tool(_) | TranscriptItem::Reasoning(_) => {
                let (turn_start, turn_end) = activity_turn_range(transcript, index);
                if final_assistant_index(transcript, turn_start, turn_end).is_none()
                    && first_activity_index(transcript, turn_start, turn_end) == Some(index)
                {
                    append_turn_activity_rows(
                        &mut rows,
                        transcript,
                        turn_start,
                        turn_end,
                        selected_tool,
                        context,
                    );
                }
                index += 1;
            }
        }
    }

    append_chat_bottom_padding(&mut rows);
    rows
}

pub(crate) fn activity_turn_range(transcript: &[TranscriptItem], index: usize) -> (usize, usize) {
    let start = transcript[..index]
        .iter()
        .rposition(|item| {
            matches!(item, TranscriptItem::Message(message) if message.role == ChatRole::User)
        })
        .map_or(0, |position| position + 1);
    let end = transcript[index + 1..]
        .iter()
        .position(|item| {
            matches!(item, TranscriptItem::Message(message) if message.role == ChatRole::User)
        })
        .map_or(transcript.len(), |offset| index + 1 + offset);
    (start, end)
}

pub(crate) fn first_activity_index(
    transcript: &[TranscriptItem],
    start: usize,
    end: usize,
) -> Option<usize> {
    transcript[start..end]
        .iter()
        .position(|item| matches!(item, TranscriptItem::Tool(_) | TranscriptItem::Reasoning(_)))
        .map(|offset| start + offset)
}

pub(crate) fn turn_has_activity(transcript: &[TranscriptItem], start: usize, end: usize) -> bool {
    transcript[start..end]
        .iter()
        .any(|item| matches!(item, TranscriptItem::Tool(_) | TranscriptItem::Reasoning(_)))
}

pub(crate) fn final_assistant_index(
    transcript: &[TranscriptItem],
    start: usize,
    end: usize,
) -> Option<usize> {
    let candidate = transcript[start..end]
        .iter()
        .rposition(|item| {
            matches!(item, TranscriptItem::Message(message) if message.role == ChatRole::Assistant)
        })
        .map(|offset| start + offset)?;
    if transcript[candidate + 1..end]
        .iter()
        .any(|item| matches!(item, TranscriptItem::Tool(_)))
    {
        None
    } else {
        Some(candidate)
    }
}

pub(crate) fn append_turn_activity_rows(
    rows: &mut Vec<TranscriptRow>,
    transcript: &[TranscriptItem],
    start: usize,
    end: usize,
    selected_tool: Option<usize>,
    context: RenderContext,
) {
    let mut lines = Vec::new();

    if context.show_reasoning
        && let Some(reasoning) = transcript[start..end]
            .iter()
            .rev()
            .find_map(|item| match item {
                TranscriptItem::Reasoning(trace) if !trace.content.trim().is_empty() => {
                    Some(trace.content.trim())
                }
                _ => None,
            })
        && let Some(latest) = reasoning.lines().rev().find(|line| !line.trim().is_empty())
    {
        lines.push(Line::from(vec![
            Span::styled("  ", muted()),
            Span::styled(
                truncate(latest.trim(), 180),
                muted().add_modifier(Modifier::ITALIC),
            ),
        ]));
    }

    append_tool_batch_lines(&mut lines, transcript, start, end, selected_tool, context);
    rows.extend(lines.into_iter().map(TranscriptRow::text));
}

pub(crate) fn append_chat_bottom_padding(rows: &mut Vec<TranscriptRow>) {
    if rows.is_empty() {
        return;
    }

    rows.extend((0..CHAT_BOTTOM_PADDING_ROWS).map(|_| TranscriptRow::text(Line::from(""))));
}

pub(crate) fn should_preserve_launch_rows(
    transcript: &[TranscriptItem],
    streaming_message: Option<usize>,
) -> bool {
    let Some(streaming_index) = streaming_message else {
        return false;
    };
    if transcript.len() > 2 {
        return false;
    }
    let Some(TranscriptItem::Message(first)) = transcript.first() else {
        return false;
    };
    if first.role != ChatRole::User {
        return false;
    }

    matches!(
        transcript.get(streaming_index),
        Some(TranscriptItem::Message(ChatMessage {
            role: ChatRole::Assistant,
            content,
            attachments,
        })) if content.is_empty() && attachments.is_empty()
    )
}

pub(crate) const WORDMARK_WIDE_MIN_COLUMNS: u16 = 64;

pub(crate) fn launch_rows() -> Vec<TranscriptRow> {
    let wide = crossterm::terminal::size()
        .map(|(width, _)| width >= WORDMARK_WIDE_MIN_COLUMNS)
        .unwrap_or(false);

    let mut lines = vec![Line::from("")];
    if wide {
        for art in [
            "  ███╗   ███╗███████╗██████╗ ██╗   ██╗███████╗ █████╗ ",
            "  ████╗ ████║██╔════╝██╔══██╗██║   ██║██╔════╝██╔══██╗",
            "  ██╔████╔██║█████╗  ██║  ██║██║   ██║███████╗███████║",
            "  ██║╚██╔╝██║██╔══╝  ██║  ██║██║   ██║╚════██║██╔══██║",
            "  ██║ ╚═╝ ██║███████╗██████╔╝╚██████╔╝███████║██║  ██║",
        ] {
            lines.push(Line::from(Span::styled(
                art,
                accent().add_modifier(Modifier::BOLD),
            )));
        }
        lines.push(Line::from(vec![
            Span::styled(
                "  ╚═╝     ╚═╝╚══════╝╚═════╝  ╚═════╝ ╚══════╝╚═╝  ╚═╝",
                accent().add_modifier(Modifier::BOLD),
            ),
            Span::styled(concat!("  v", env!("CARGO_PKG_VERSION")), muted()),
        ]));
    } else {
        lines.push(Line::from(Span::styled(
            "  █▀▄▀█ █▀▀ █▀▄ █░█ █▀ ▄▀█",
            accent().add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(vec![
            Span::styled(
                "  █░▀░█ ██▄ █▄▀ █▄█ ▄█ █▀█",
                accent().add_modifier(Modifier::BOLD),
            ),
            Span::styled(concat!("  v", env!("CARGO_PKG_VERSION")), muted()),
        ]));
    }

    lines.extend([
        Line::from(""),
        Line::from(vec![Span::styled(
            "  the coding agent that plans, edits, and verifies",
            value_style(),
        )]),
        Line::from(""),
        Line::from(vec![
            Span::styled("  enter", prompt_style()),
            Span::styled(" send a task", muted()),
            Span::styled("      shift+tab", prompt_style()),
            Span::styled(" plan mode", muted()),
            Span::styled("      ctrl+p", prompt_style()),
            Span::styled(" commands", muted()),
        ]),
        Line::from(vec![
            Span::styled("  ctrl+i", prompt_style()),
            Span::styled(" paste image", muted()),
            Span::styled("    /workflow", prompt_style()),
            Span::styled(" agent fleet", muted()),
            Span::styled("     esc esc", prompt_style()),
            Span::styled(" quit", muted()),
        ]),
        Line::from(""),
    ]);

    lines.into_iter().map(TranscriptRow::text).collect()
}

pub(crate) fn transcript_lines_from_rows(rows: &[TranscriptRow]) -> Vec<Line<'static>> {
    rows.iter().map(|row| row.line.clone()).collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TranscriptImagePlacement {
    pub(crate) attachment: ImageAttachment,
    pub(crate) width: u16,
    pub(crate) height: u16,
    pub(crate) x_offset: u16,
    pub(crate) y_offset: i16,
}

pub(crate) fn transcript_image_placements(
    rows: &[TranscriptRow],
    area: Rect,
    top_offset: usize,
) -> Vec<TranscriptImagePlacement> {
    if area.width < 8 || area.height == 0 {
        return Vec::new();
    }

    let x_offset = 2;
    let image_width = CHAT_IMAGE_PREVIEW_WIDTH.min(area.width.saturating_sub(x_offset));
    if image_width == 0 {
        return Vec::new();
    }

    let viewport_bottom = top_offset.saturating_add(area.height as usize);
    let mut placements = Vec::new();
    let mut visual_start = 0usize;

    for row in rows {
        if let Some(attachment) = &row.image {
            let image_height = CHAT_IMAGE_PREVIEW_HEIGHT;
            let image_bottom = visual_start.saturating_add(image_height as usize);
            if image_bottom > top_offset && visual_start < viewport_bottom {
                placements.push(TranscriptImagePlacement {
                    attachment: attachment.clone(),
                    width: image_width,
                    height: image_height,
                    x_offset,
                    y_offset: signed_visual_offset(visual_start, top_offset),
                });
            }
        }

        visual_start = visual_start.saturating_add(row_visual_height(row, area.width));
        if visual_start >= viewport_bottom {
            break;
        }
    }

    placements
}

pub(crate) fn signed_visual_offset(visual_start: usize, top_offset: usize) -> i16 {
    if visual_start >= top_offset {
        visual_start
            .saturating_sub(top_offset)
            .min(i16::MAX as usize) as i16
    } else {
        -(top_offset
            .saturating_sub(visual_start)
            .min(i16::MAX as usize) as i16)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct TranscriptViewportWindow {
    pub(crate) rows: Vec<TranscriptRow>,
    pub(crate) scroll_offset: usize,
}

pub(crate) fn transcript_viewport_window(
    rows: &[TranscriptRow],
    width: u16,
    top_offset: usize,
    viewport_height: usize,
) -> TranscriptViewportWindow {
    if rows.is_empty() || viewport_height == 0 {
        return TranscriptViewportWindow {
            rows: Vec::new(),
            scroll_offset: 0,
        };
    }

    let mut skipped_visual_rows = 0usize;
    let mut visible_visual_rows = 0usize;
    let mut scroll_offset = 0usize;
    let mut visible_rows = Vec::new();
    let mut taking = false;

    for row in rows {
        let visual_rows = row_visual_height(row, width);
        if !taking {
            if skipped_visual_rows.saturating_add(visual_rows) <= top_offset {
                skipped_visual_rows = skipped_visual_rows.saturating_add(visual_rows);
                continue;
            }
            scroll_offset = top_offset.saturating_sub(skipped_visual_rows);
            taking = true;
        }

        visible_rows.push(row.clone());
        visible_visual_rows = visible_visual_rows.saturating_add(visual_rows);
        if visible_visual_rows.saturating_sub(scroll_offset) >= viewport_height {
            break;
        }
    }

    TranscriptViewportWindow {
        rows: visible_rows,
        scroll_offset,
    }
}

pub(crate) fn chat_viewport_metrics(
    rows: &[TranscriptRow],
    area: Rect,
    requested_scroll: usize,
) -> ChatViewportMetrics {
    let text_area = area;
    let total_visual_lines = wrapped_row_count(rows, text_area.width);
    let has_scrollbar = area.width > 4 && total_visual_lines > area.height as usize;
    let max_scroll = total_visual_lines.saturating_sub(area.height as usize);
    let scroll = requested_scroll.min(max_scroll);
    let top_offset = max_scroll.saturating_sub(scroll);

    ChatViewportMetrics {
        text_area,
        has_scrollbar,
        total_visual_lines,
        max_scroll,
        scroll,
        top_offset,
    }
}

pub(crate) fn scroll_progress_percent(metrics: &ChatViewportMetrics) -> usize {
    if metrics.max_scroll == 0 {
        return 100;
    }
    metrics.top_offset.saturating_mul(100) / metrics.max_scroll
}

pub(crate) fn paragraph_scroll_offset(top_offset: usize) -> u16 {
    top_offset.min(u16::MAX as usize) as u16
}

pub(crate) fn wrapped_row_count(rows: &[TranscriptRow], width: u16) -> usize {
    rows.iter().map(|row| row_visual_height(row, width)).sum()
}

pub(crate) fn row_visual_height(row: &TranscriptRow, width: u16) -> usize {
    let width = width.max(1) as usize;
    if row.image.is_some() {
        1
    } else {
        line_width(&row.line).max(1).div_ceil(width)
    }
}

#[cfg(test)]
pub(crate) fn trim_wrapped_lines_for_viewport(
    rows: &[TranscriptRow],
    width: u16,
    skip_rows: usize,
    viewport_height: usize,
) -> Vec<TranscriptRow> {
    transcript_viewport_window(rows, width, skip_rows, viewport_height).rows
}

pub(crate) fn line_width(line: &Line<'_>) -> usize {
    line.spans
        .iter()
        .map(|span| span.content.chars().map(char_display_width).sum::<usize>())
        .sum()
}

pub(crate) fn char_display_width(ch: char) -> usize {
    if ch == '\n' || ch == '\r' || ch == '\t' {
        1
    } else if ch.is_control() {
        0
    } else {
        1
    }
}

pub(crate) fn append_chat_message_rows(
    rows: &mut Vec<TranscriptRow>,
    message: &ChatMessage,
    is_streaming: bool,
) {
    if message.role == ChatRole::User {
        append_user_message_rows(rows, &message.content, &message.attachments);
        return;
    }

    let mut lines = Vec::new();
    append_chat_message_lines(&mut lines, message, is_streaming);
    rows.extend(lines.into_iter().map(TranscriptRow::text));
}

pub(crate) fn append_chat_message_lines(
    lines: &mut Vec<Line<'static>>,
    message: &ChatMessage,
    is_streaming: bool,
) {
    if message.role == ChatRole::User {
        append_user_message_lines(lines, &message.content, &message.attachments);
        return;
    }

    if message.role == ChatRole::System {
        for line in message
            .content
            .lines()
            .filter(|line| !line.trim().is_empty())
        {
            lines.push(Line::from(vec![
                Span::styled("  · ", muted()),
                Span::styled(line.trim().to_string(), muted()),
            ]));
        }
        return;
    }

    if message.content.is_empty() && is_streaming {
        return;
    }

    let mut rendered = markdown_content_lines(&message.content, message.role);
    if is_streaming {
        if let Some(last) = rendered.last_mut() {
            last.spans.push(Span::styled("█", cursor_style()));
        } else {
            rendered.push(Line::from(Span::styled("  █", cursor_style())));
        }
    }

    lines.extend(rendered);
}

pub(crate) fn append_user_message_rows(
    rows: &mut Vec<TranscriptRow>,
    content: &str,
    attachments: &[ImageAttachment],
) {
    let mut lines = Vec::new();
    append_user_message_lines(&mut lines, content, attachments);
    rows.extend(lines.into_iter().map(TranscriptRow::text));

    for attachment in attachments {
        let preview = image_placeholder_lines(
            attachment,
            CHAT_IMAGE_PREVIEW_WIDTH,
            CHAT_IMAGE_PREVIEW_HEIGHT,
        );
        for (index, line) in preview.into_iter().enumerate() {
            if index == 0 {
                rows.push(TranscriptRow::image(line, attachment.clone()));
            } else {
                rows.push(TranscriptRow::text(line));
            }
        }
    }
}

pub(crate) fn append_user_message_lines(
    lines: &mut Vec<Line<'static>>,
    content: &str,
    attachments: &[ImageAttachment],
) {
    let base_style = user_message_style();
    let prompt_style = user_message_prompt_style();

    if content.trim().is_empty() {
        lines.push(Line::from(vec![
            Span::styled(" › ", prompt_style),
            Span::styled(" ", base_style),
        ]));
        return;
    }

    for (index, raw_line) in content.lines().enumerate() {
        let marker = if index == 0 { " › " } else { "   " };
        let mut spans = vec![Span::styled(marker, prompt_style)];
        spans.extend(inline_markdown_spans(raw_line.trim_end(), base_style));
        spans.push(Span::styled(" ", base_style));
        lines.push(Line::from(spans));
    }

    if !attachments.is_empty() {
        lines.push(attachment_strip_line(attachments).style(user_message_background_style()));
    }
}
