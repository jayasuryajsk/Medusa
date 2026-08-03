use std::{
    collections::HashMap,
    sync::{OnceLock, atomic::Ordering},
};

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::config::ACTIVE_THEME;
use crate::styles::{
    code_block_style, code_border_style, heading_style, inline_code_style, link_style,
    list_marker_style, message_style, muted, quote_border_style, quote_style, separator_style,
};
use crate::terminal::{horizontal_rule, ui_border_set};
use crate::types::ChatRole;

pub(crate) fn code_syntaxes() -> &'static syntect::parsing::SyntaxSet {
    static SYNTAXES: OnceLock<syntect::parsing::SyntaxSet> = OnceLock::new();
    SYNTAXES.get_or_init(syntect::parsing::SyntaxSet::load_defaults_newlines)
}

pub(crate) fn code_theme() -> &'static syntect::highlighting::Theme {
    static THEME: OnceLock<syntect::highlighting::Theme> = OnceLock::new();
    THEME.get_or_init(|| {
        // Muted base16 palette that sits well on all of Medusa's dark themes.
        // Only foreground colors are used; the terminal background shows through.
        syntect::highlighting::ThemeSet::load_defaults()
            .themes
            .remove("base16-eighties.dark")
            .expect("syntect default themes include base16-eighties.dark")
    })
}

pub(crate) struct CodeHighlighter {
    inner: syntect::easy::HighlightLines<'static>,
}

impl CodeHighlighter {
    /// None when the fence has no language tag or we don't know the syntax —
    /// the block then renders in the plain code style.
    pub(crate) fn for_language(token: &str) -> Option<Self> {
        let token = token.trim();
        if token.is_empty() {
            return None;
        }
        let syntaxes = code_syntaxes();
        let syntax = syntaxes
            .find_syntax_by_token(token)
            .or_else(|| syntaxes.find_syntax_by_extension(token))?;
        Some(Self {
            inner: syntect::easy::HighlightLines::new(syntax, code_theme()),
        })
    }

    pub(crate) fn spans(&mut self, line: &str) -> Vec<Span<'static>> {
        let with_newline = format!("{line}\n");
        let Ok(regions) = self.inner.highlight_line(&with_newline, code_syntaxes()) else {
            return vec![Span::styled(line.to_string(), code_block_style())];
        };
        regions
            .into_iter()
            .map(|(style, text)| {
                let fg = style.foreground;
                Span::styled(
                    text.trim_end_matches('\n').to_string(),
                    Style::default().fg(Color::Rgb(fg.r, fg.g, fg.b)),
                )
            })
            .filter(|span| !span.content.is_empty())
            .collect()
    }
}

/// Memoized front for [`markdown_content_lines_uncached`]. During streaming
/// every delta invalidates the whole-transcript row cache, which would
/// re-render (and re-highlight) every historical message per frame; this keeps
/// that cost to the one message actually changing.
pub(crate) fn markdown_content_lines(content: &str, role: ChatRole) -> Vec<Line<'static>> {
    use std::hash::{Hash, Hasher};

    const MARKDOWN_CACHE_CAP: usize = 512;
    static CACHE: OnceLock<std::sync::Mutex<HashMap<u64, Vec<Line<'static>>>>> = OnceLock::new();

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    content.hash(&mut hasher);
    (role as u8).hash(&mut hasher);
    ACTIVE_THEME.load(Ordering::Relaxed).hash(&mut hasher);
    let key = hasher.finish();

    let cache = CACHE.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    if let Ok(cache) = cache.lock()
        && let Some(lines) = cache.get(&key)
    {
        return lines.clone();
    }

    let lines = markdown_content_lines_uncached(content, role);
    if let Ok(mut cache) = cache.lock() {
        // Streaming generates a new key per delta; a full reset at the cap is
        // fine because live entries repopulate on the next frame.
        if cache.len() >= MARKDOWN_CACHE_CAP {
            cache.clear();
        }
        cache.insert(key, lines.clone());
    }
    lines
}

pub(crate) fn markdown_content_lines_uncached(content: &str, role: ChatRole) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut in_code_block = false;
    let mut highlighter: Option<CodeHighlighter> = None;

    for raw_line in content.lines() {
        let line = raw_line.trim_end();
        let trimmed = line.trim_start();

        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_code_block = !in_code_block;
            highlighter = if in_code_block {
                CodeHighlighter::for_language(&trimmed[3..])
            } else {
                None
            };
            continue;
        }

        if in_code_block {
            let mut spans = vec![Span::styled(
                format!("  {} ", ui_border_set().vertical_left),
                code_border_style(),
            )];
            match highlighter.as_mut() {
                Some(highlighter) => spans.extend(highlighter.spans(line)),
                None => spans.push(Span::styled(line.to_string(), code_block_style())),
            }
            lines.push(Line::from(spans));
            continue;
        }

        if trimmed.is_empty() {
            lines.push(Line::from(""));
            continue;
        }

        if is_horizontal_rule(trimmed) {
            lines.push(Line::from(vec![
                Span::styled("  ", muted()),
                Span::styled(horizontal_rule(48), separator_style()),
            ]));
            continue;
        }

        if let Some((level, heading)) = parse_heading(trimmed) {
            lines.push(Line::from(vec![
                Span::styled("  ", muted()),
                Span::styled(heading.to_string(), heading_style(level)),
            ]));
            continue;
        }

        if let Some(quote) = trimmed.strip_prefix("> ") {
            let mut spans = vec![
                Span::styled(
                    format!("  {} ", ui_border_set().vertical_left),
                    quote_border_style(),
                ),
                Span::styled("", quote_style()),
            ];
            spans.extend(inline_markdown_spans(quote, quote_style()));
            lines.push(Line::from(spans));
            continue;
        }

        if let Some((indent, marker, body)) = parse_list_item(line) {
            let mut spans = vec![
                Span::styled("  ", muted()),
                Span::raw("  ".repeat(indent)),
                Span::styled(marker, list_marker_style()),
                Span::raw(" "),
            ];
            spans.extend(inline_markdown_spans(body, message_style(role)));
            lines.push(Line::from(spans));
            continue;
        }

        let mut spans = vec![Span::styled("  ", muted())];
        spans.extend(inline_markdown_spans(trimmed, message_style(role)));
        lines.push(Line::from(spans));
    }

    lines
}

pub(crate) fn parse_heading(line: &str) -> Option<(usize, &str)> {
    let level = line.chars().take_while(|ch| *ch == '#').count();
    if !(1..=6).contains(&level) {
        return None;
    }

    let rest = line.get(level..)?;
    if !rest.starts_with(' ') {
        return None;
    }

    Some((level, rest.trim()))
}

pub(crate) fn parse_list_item(line: &str) -> Option<(usize, String, &str)> {
    let leading_spaces = line.chars().take_while(|ch| *ch == ' ').count();
    let indent = leading_spaces / 2;
    let trimmed = line.trim_start();

    for marker in ["- ", "* ", "+ "] {
        if let Some(body) = trimmed.strip_prefix(marker) {
            return Some((indent, "•".to_string(), body.trim()));
        }
    }

    let dot = trimmed.find(". ")?;
    let number = &trimmed[..dot];
    if !number.chars().all(|ch| ch.is_ascii_digit()) {
        return None;
    }

    Some((indent, format!("{number}."), trimmed[dot + 2..].trim()))
}

pub(crate) fn is_horizontal_rule(line: &str) -> bool {
    let chars = line.chars().collect::<Vec<_>>();
    chars.len() >= 3 && chars.iter().all(|ch| matches!(ch, '-' | '*' | '_'))
}

pub(crate) fn inline_markdown_spans(text: &str, base_style: Style) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut rest = text;

    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix('`')
            && let Some(end) = after.find('`')
        {
            spans.push(Span::styled(after[..end].to_string(), inline_code_style()));
            rest = &after[end + 1..];
            continue;
        }

        if let Some(after) = rest.strip_prefix("**")
            && let Some(end) = after.find("**")
        {
            spans.push(Span::styled(
                after[..end].to_string(),
                base_style.add_modifier(Modifier::BOLD),
            ));
            rest = &after[end + 2..];
            continue;
        }

        if let Some(after) = rest.strip_prefix("__")
            && let Some(end) = after.find("__")
        {
            spans.push(Span::styled(
                after[..end].to_string(),
                base_style.add_modifier(Modifier::BOLD),
            ));
            rest = &after[end + 2..];
            continue;
        }

        if let Some(after) = rest.strip_prefix('*')
            && let Some(end) = after.find('*')
        {
            spans.push(Span::styled(
                after[..end].to_string(),
                base_style.add_modifier(Modifier::ITALIC),
            ));
            rest = &after[end + 1..];
            continue;
        }

        if let Some(after) = rest.strip_prefix('_')
            && let Some(end) = after.find('_')
        {
            spans.push(Span::styled(
                after[..end].to_string(),
                base_style.add_modifier(Modifier::ITALIC),
            ));
            rest = &after[end + 1..];
            continue;
        }

        if let Some(after_open) = rest.strip_prefix('[')
            && let Some(close) = after_open.find("](")
        {
            let label = &after_open[..close];
            let after_label = &after_open[close + 2..];
            if let Some(end_url) = after_label.find(')') {
                spans.push(Span::styled(label.to_string(), link_style()));
                rest = &after_label[end_url + 1..];
                continue;
            }
        }

        let next = next_inline_marker(rest).unwrap_or(rest.len()).max(1);
        spans.push(Span::styled(rest[..next].to_string(), base_style));
        rest = &rest[next..];
    }

    spans
}

pub(crate) fn next_inline_marker(text: &str) -> Option<usize> {
    ["`", "**", "__", "*", "_", "["]
        .iter()
        .filter_map(|marker| text.find(marker))
        .filter(|index| *index > 0)
        .min()
}
