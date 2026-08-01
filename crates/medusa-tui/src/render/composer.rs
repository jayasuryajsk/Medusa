use super::*;

pub(crate) fn input_display_lines(
    input: &str,
    cursor: usize,
    max_lines: usize,
) -> Vec<Line<'static>> {
    if input.is_empty() {
        return vec![Line::from(vec![
            Span::styled("█", cursor_style()),
            Span::styled(" Type a task or ask a question…", placeholder_style()),
        ])];
    }

    let max_lines = max_lines.max(1);
    let visible_start_line = cursor_line(input, cursor)
        .saturating_add(1)
        .saturating_sub(max_lines);

    let mut lines = Vec::new();
    let mut current = Vec::new();
    let mut current_line = 0usize;

    for (index, ch) in input.chars().enumerate() {
        if current_line >= visible_start_line && index == cursor {
            current.push(Span::styled("█", cursor_style()));
        }

        if ch == '\n' {
            if current_line >= visible_start_line {
                lines.push(Line::from(current));
                current = Vec::new();
                if lines.len() >= max_lines {
                    return lines;
                }
            }
            current_line += 1;
        } else if current_line >= visible_start_line {
            current.push(Span::styled(ch.to_string(), value_style()));
        }
    }

    if current_line >= visible_start_line && cursor == input.chars().count() {
        current.push(Span::styled("█", cursor_style()));
    }

    if current_line >= visible_start_line && lines.len() < max_lines {
        lines.push(Line::from(current));
    }
    lines
}

pub(crate) fn cursor_line(input: &str, cursor: usize) -> usize {
    input.chars().take(cursor).filter(|ch| *ch == '\n').count()
}

pub(crate) fn vertically_center_input_lines(
    mut lines: Vec<Line<'static>>,
    available_content_height: u16,
) -> Vec<Line<'static>> {
    let available = available_content_height as usize;
    if available <= lines.len() {
        return lines;
    }

    let top_padding = (available - lines.len()).div_ceil(2);
    if top_padding == 0 {
        return lines;
    }

    let mut centered = vec![Line::from(""); top_padding];
    centered.append(&mut lines);
    centered
}

pub(crate) fn attachment_strip_line(attachments: &[ImageAttachment]) -> Line<'static> {
    let mut spans = vec![Span::styled("  ", muted())];
    for (index, attachment) in attachments.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw(" "));
        }
        spans.push(Span::styled(
            format!("󰋩 {}", attachment_label(attachment)),
            Style::default()
                .fg(palette().success)
                .bg(palette().inline_code_bg)
                .add_modifier(Modifier::BOLD),
        ));
    }
    spans.push(Span::styled("  ctrl+d detach latest", muted()));
    Line::from(spans)
}

pub(crate) fn composer_attachment_preview_lines(
    attachments: &[ImageAttachment],
    previews: &HashMap<String, Vec<Line<'static>>>,
    available_width: u16,
) -> Vec<Line<'static>> {
    if attachments.is_empty() || available_width == 0 {
        return Vec::new();
    }

    let cell_width = COMPOSER_IMAGE_PREVIEW_WIDTH as usize;
    let gap_width = 1usize;
    let usable_width = available_width.saturating_sub(4) as usize;
    let max_cells = (usable_width + gap_width)
        .checked_div(cell_width + gap_width)
        .unwrap_or(1)
        .max(1);
    let visible_count = attachments.len().min(max_cells);
    let hidden_count = attachments.len().saturating_sub(visible_count);
    let mut rows = vec![Vec::new(); COMPOSER_IMAGE_PREVIEW_HEIGHT as usize];

    for attachment in attachments.iter().take(visible_count) {
        let fallback;
        let preview = if let Some(preview) = previews.get(&attachment.id) {
            preview
        } else {
            fallback = image_preview_lines(attachment, COMPOSER_IMAGE_PREVIEW_WIDTH);
            &fallback
        };

        for (row_index, row) in rows.iter_mut().enumerate() {
            if !row.is_empty() {
                row.push(Span::raw(" "));
            }
            if let Some(line) = preview.get(row_index) {
                row.extend(line.spans.clone());
            } else {
                row.push(Span::raw(" ".repeat(cell_width)));
            }
        }
    }

    if hidden_count > 0
        && let Some(first_row) = rows.first_mut()
    {
        first_row.push(Span::raw(" "));
        first_row.push(Span::styled(
            format!("+{hidden_count}"),
            attachment_preview_meta_style(),
        ));
    }

    rows.into_iter()
        .map(|spans| {
            let mut prefixed = vec![Span::styled("  ", muted())];
            prefixed.extend(spans);
            Line::from(prefixed)
        })
        .collect()
}

pub(crate) fn image_preview_lines(attachment: &ImageAttachment, width: u16) -> Vec<Line<'static>> {
    image_placeholder_lines(attachment, width, COMPOSER_IMAGE_PREVIEW_HEIGHT)
}

pub(crate) fn image_input_warning(images_supported: bool) -> Option<&'static str> {
    if images_supported {
        None
    } else {
        Some("selected model does not accept image input")
    }
}

pub(crate) fn preview_image_dimensions(
    attachment: &ImageAttachment,
    area: Rect,
    zoom: u16,
) -> (u16, u16) {
    if area.width == 0 || area.height == 0 {
        return (0, 0);
    }

    let image_width = attachment.width.max(1) as f64;
    let image_height = attachment.height.max(1) as f64;
    let fit_height_from_width = ((area.width as f64 * image_height / image_width) * 0.5)
        .ceil()
        .max(1.0) as u16;

    let (fit_width, fit_height) = if fit_height_from_width <= area.height {
        (area.width, fit_height_from_width)
    } else {
        let width = ((area.height as f64 * image_width / image_height) * 2.0)
            .ceil()
            .max(1.0) as u16;
        (width.min(area.width), area.height)
    };

    let zoom = zoom.clamp(IMAGE_PREVIEW_MIN_ZOOM, IMAGE_PREVIEW_MAX_ZOOM) as u32;
    let width = ((fit_width as u32).saturating_mul(zoom) / 100).clamp(1, u16::MAX as u32) as u16;
    let height = ((fit_height as u32).saturating_mul(zoom) / 100).clamp(1, u16::MAX as u32) as u16;

    (width, height)
}

pub(crate) fn image_placeholder_lines(
    attachment: &ImageAttachment,
    width: u16,
    height: u16,
) -> Vec<Line<'static>> {
    let width = width.max(10) as usize;
    let height = height.max(3) as usize;
    let inner_width = width.saturating_sub(2);
    let border = "─".repeat(inner_width);
    let dimensions = format!("{}×{}", attachment.width, attachment.height);
    let size = human_bytes(attachment.size_bytes);
    let name = truncate(&attachment.name, inner_width);
    let mut lines = vec![
        Line::from(Span::styled(
            format!("╭{border}╮"),
            attachment_preview_border_style(),
        )),
        attachment_preview_body_line("image", width, attachment_preview_title_style()),
        attachment_preview_body_line(&dimensions, width, attachment_preview_meta_style()),
    ];

    while lines.len() + 2 < height {
        lines.push(attachment_preview_body_line("", width, muted()));
    }

    lines.push(attachment_preview_body_line(
        &format!("{name} {size}"),
        width,
        muted(),
    ));
    lines.push(Line::from(Span::styled(
        format!("╰{border}╯"),
        attachment_preview_border_style(),
    )));
    lines
}

pub(crate) fn attachment_preview_body_line(
    text: &str,
    width: usize,
    style: Style,
) -> Line<'static> {
    let inner_width = width.saturating_sub(2);
    let fitted = truncate(text, inner_width);
    let padding = inner_width.saturating_sub(fitted.chars().count());
    Line::from(vec![
        Span::styled("│", attachment_preview_border_style()),
        Span::styled(fitted, style),
        Span::raw(" ".repeat(padding)),
        Span::styled("│", attachment_preview_border_style()),
    ])
}
