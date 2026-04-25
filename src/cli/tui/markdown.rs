//! Lightweight Markdown renderer for assistant output.
//!
//! The goal is not full CommonMark. It covers the shapes agent replies
//! commonly emit and keeps the presentation close to Codex / Claude
//! Code: default text first, dim structure, sparse accent color, no
//! filled message backgrounds.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

pub(super) struct Engine;

impl Engine {
    pub(super) fn render_into<'a>(out: &mut Vec<Line<'a>>, text: &str) {
        let mut in_code = false;

        for raw in text.split('\n') {
            let trimmed = raw.trim_start();

            if let Some(info) = fence_info(trimmed) {
                if in_code {
                    push_code_end(out);
                    in_code = false;
                } else {
                    in_code = true;
                    push_code_start(out, info);
                }
                continue;
            }

            if in_code {
                push_code_line(out, raw);
            } else {
                push_markdown_line(out, raw);
            }
        }
    }

    pub(super) fn line_count(text: &str) -> usize {
        let mut lines: Vec<Line<'static>> = Vec::new();
        Self::render_into(&mut lines, text);
        lines.len()
    }
}

fn push_markdown_line<'a>(out: &mut Vec<Line<'a>>, raw: &str) {
    let trimmed = raw.trim_start();
    let indent_len = raw.len().saturating_sub(trimmed.len());
    let indent = &raw[..indent_len];

    if trimmed.is_empty() {
        out.push(Line::raw(""));
        return;
    }

    if let Some((level, rest)) = parse_heading(trimmed) {
        let style = heading_style(level);
        let mut spans = Vec::new();
        push_plain(&mut spans, indent, Style::default());
        spans.push(Span::styled(
            format!("{} ", "#".repeat(level)),
            accent_style().add_modifier(Modifier::BOLD),
        ));
        spans.extend(inline_spans(rest, style));
        out.push(Line::from(spans));
        return;
    }

    if is_rule(trimmed) {
        out.push(Line::from(vec![
            Span::raw(indent.to_string()),
            Span::styled("────────────────────────", dim_style()),
        ]));
        return;
    }

    if let Some((level, rest)) = parse_quote(trimmed) {
        let mut spans = Vec::new();
        push_plain(&mut spans, indent, Style::default());
        for _ in 0..level {
            spans.push(Span::styled("│ ", dim_style()));
        }
        spans.extend(inline_spans(rest, dim_style()));
        out.push(Line::from(spans));
        return;
    }

    if let Some(rest) = parse_unordered_item(trimmed) {
        push_prefixed_inline(out, indent, "• ", rest);
        return;
    }

    if let Some((marker, rest)) = parse_ordered_item(trimmed) {
        push_prefixed_inline(out, indent, marker, rest);
        return;
    }

    let mut spans = Vec::new();
    push_plain(&mut spans, indent, Style::default());
    spans.extend(inline_spans(trimmed, Style::default()));
    out.push(Line::from(spans));
}

fn push_code_line<'a>(out: &mut Vec<Line<'a>>, raw: &str) {
    out.push(Line::from(vec![
        Span::styled("  │ ", dim_style()),
        Span::styled(raw.to_string(), code_style()),
    ]));
}

fn push_code_start<'a>(out: &mut Vec<Line<'a>>, info: &str) {
    let title = if info.is_empty() { "code" } else { info };
    out.push(Line::from(vec![
        Span::styled("  ╭─ ", dim_style()),
        Span::styled(
            title.to_string(),
            dim_style().add_modifier(Modifier::ITALIC),
        ),
    ]));
}

fn push_code_end<'a>(out: &mut Vec<Line<'a>>) {
    out.push(Line::from(vec![Span::styled("  ╰─", dim_style())]));
}

fn push_prefixed_inline<'a>(out: &mut Vec<Line<'a>>, indent: &str, marker: &str, rest: &str) {
    let mut spans = Vec::new();
    push_plain(&mut spans, indent, Style::default());
    spans.push(Span::styled(marker.to_string(), dim_style()));
    spans.extend(inline_spans(rest, Style::default()));
    out.push(Line::from(spans));
}

fn inline_spans<'a>(text: &str, base: Style) -> Vec<Span<'a>> {
    let mut spans = Vec::new();
    let mut plain_start = 0usize;
    let mut pos = 0usize;
    let bytes = text.as_bytes();

    while pos < bytes.len() {
        if bytes[pos] == b'`'
            && let Some(end) = find_byte(bytes, pos + 1, b'`')
        {
            push_plain(&mut spans, &text[plain_start..pos], base);
            spans.push(Span::styled(text[pos + 1..end].to_string(), code_style()));
            pos = end + 1;
            plain_start = pos;
            continue;
        }

        if starts_with_at(text, pos, "**")
            && let Some(end) = find_from(text, pos + 2, "**")
        {
            push_plain(&mut spans, &text[plain_start..pos], base);
            spans.extend(inline_spans(
                &text[pos + 2..end],
                base.add_modifier(Modifier::BOLD),
            ));
            pos = end + 2;
            plain_start = pos;
            continue;
        }

        if starts_with_at(text, pos, "__")
            && let Some(end) = find_from(text, pos + 2, "__")
        {
            push_plain(&mut spans, &text[plain_start..pos], base);
            spans.extend(inline_spans(
                &text[pos + 2..end],
                base.add_modifier(Modifier::BOLD),
            ));
            pos = end + 2;
            plain_start = pos;
            continue;
        }

        if bytes[pos] == b'['
            && let Some((label_end, url_end)) = parse_link(text, pos)
        {
            push_plain(&mut spans, &text[plain_start..pos], base);
            let label = &text[pos + 1..label_end];
            let url = &text[label_end + 2..url_end];
            let display = if label.is_empty() { url } else { label };
            spans.push(Span::styled(
                display.to_string(),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::UNDERLINED),
            ));
            pos = url_end + 1;
            plain_start = pos;
            continue;
        }

        if bytes[pos] == b'*'
            && !starts_with_at(text, pos, "**")
            && let Some(end) = find_single_delim(text, pos + 1, '*')
        {
            push_plain(&mut spans, &text[plain_start..pos], base);
            spans.extend(inline_spans(
                &text[pos + 1..end],
                base.add_modifier(Modifier::ITALIC),
            ));
            pos = end + 1;
            plain_start = pos;
            continue;
        }

        if bytes[pos] == b'_'
            && !starts_with_at(text, pos, "__")
            && !is_word_byte_before(bytes, pos)
            && let Some(end) = find_single_delim(text, pos + 1, '_')
        {
            push_plain(&mut spans, &text[plain_start..pos], base);
            spans.extend(inline_spans(
                &text[pos + 1..end],
                base.add_modifier(Modifier::ITALIC),
            ));
            pos = end + 1;
            plain_start = pos;
            continue;
        }

        pos += 1;
    }

    push_plain(&mut spans, &text[plain_start..], base);
    spans
}

fn push_plain<'a>(spans: &mut Vec<Span<'a>>, text: &str, style: Style) {
    if !text.is_empty() {
        spans.push(Span::styled(text.to_string(), style));
    }
}

fn fence_info(trimmed: &str) -> Option<&str> {
    trimmed
        .strip_prefix("```")
        .or_else(|| trimmed.strip_prefix("~~~"))
        .map(str::trim)
}

fn parse_heading(trimmed: &str) -> Option<(usize, &str)> {
    let level = trimmed.bytes().take_while(|b| *b == b'#').count();
    if !(1..=6).contains(&level) {
        return None;
    }
    if trimmed.as_bytes().get(level) != Some(&b' ') {
        return None;
    }
    Some((level, &trimmed[level + 1..]))
}

fn is_rule(trimmed: &str) -> bool {
    let mut chars = trimmed.chars().filter(|c| !c.is_whitespace());
    let Some(first) = chars.next() else {
        return false;
    };
    if !matches!(first, '-' | '*' | '_') {
        return false;
    }
    let mut count = 1usize;
    for ch in chars {
        if ch != first {
            return false;
        }
        count += 1;
    }
    count >= 3
}

fn parse_quote(mut trimmed: &str) -> Option<(usize, &str)> {
    let mut level = 0usize;
    loop {
        trimmed = trimmed.trim_start();
        let Some(rest) = trimmed.strip_prefix('>') else {
            break;
        };
        level += 1;
        trimmed = rest.strip_prefix(' ').unwrap_or(rest);
    }
    (level > 0).then_some((level, trimmed))
}

fn parse_unordered_item(trimmed: &str) -> Option<&str> {
    let bytes = trimmed.as_bytes();
    if bytes.len() >= 2 && matches!(bytes[0], b'-' | b'*' | b'+') && bytes[1] == b' ' {
        Some(&trimmed[2..])
    } else {
        None
    }
}

fn parse_ordered_item(trimmed: &str) -> Option<(&str, &str)> {
    let bytes = trimmed.as_bytes();
    let digits = bytes.iter().take_while(|b| b.is_ascii_digit()).count();
    if digits == 0 || digits > 3 {
        return None;
    }
    if !matches!(bytes.get(digits), Some(b'.' | b')')) || bytes.get(digits + 1) != Some(&b' ') {
        return None;
    }
    Some((&trimmed[..digits + 2], &trimmed[digits + 2..]))
}

fn parse_link(text: &str, pos: usize) -> Option<(usize, usize)> {
    let label_end = pos + 1 + text[pos + 1..].find(']')?;
    if !starts_with_at(text, label_end, "](") {
        return None;
    }
    let url_start = label_end + 2;
    let url_end = url_start + text[url_start..].find(')')?;
    Some((label_end, url_end))
}

fn find_byte(bytes: &[u8], from: usize, needle: u8) -> Option<usize> {
    bytes
        .get(from..)?
        .iter()
        .position(|b| *b == needle)
        .map(|i| from + i)
}

fn find_from(text: &str, from: usize, needle: &str) -> Option<usize> {
    text.get(from..)?.find(needle).map(|i| from + i)
}

fn find_single_delim(text: &str, from: usize, marker: char) -> Option<usize> {
    for (offset, ch) in text.get(from..)?.char_indices() {
        let idx = from + offset;
        if ch == marker && !starts_with_at(text, idx, &marker.to_string().repeat(2)) {
            return Some(idx);
        }
    }
    None
}

fn starts_with_at(text: &str, pos: usize, needle: &str) -> bool {
    text.get(pos..).is_some_and(|rest| rest.starts_with(needle))
}

fn is_word_byte_before(bytes: &[u8], pos: usize) -> bool {
    pos > 0 && bytes[pos - 1].is_ascii_alphanumeric()
}

fn dim_style() -> Style {
    Style::default().fg(Color::DarkGray)
}

fn accent_style() -> Style {
    Style::default().fg(Color::Cyan)
}

fn code_style() -> Style {
    Style::default().fg(Color::Cyan)
}

fn heading_style(level: usize) -> Style {
    match level {
        1 => Style::default()
            .add_modifier(Modifier::BOLD)
            .add_modifier(Modifier::UNDERLINED),
        2 => Style::default().add_modifier(Modifier::BOLD),
        3 => Style::default()
            .add_modifier(Modifier::BOLD)
            .add_modifier(Modifier::ITALIC),
        _ => Style::default().add_modifier(Modifier::ITALIC),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render_to_strings(input: &str) -> Vec<String> {
        let mut lines = Vec::new();
        Engine::render_into(&mut lines, input);
        lines.into_iter().map(|line| line.to_string()).collect()
    }

    #[test]
    fn hides_code_fences_but_keeps_code_body() {
        assert_eq!(
            render_to_strings("```rust\nfn main() {}\n```"),
            vec!["  ╭─ rust", "  │ fn main() {}", "  ╰─"]
        );
    }

    #[test]
    fn renders_common_block_shapes() {
        assert_eq!(
            render_to_strings("# Title\n- item\n1. next\n> quote"),
            vec!["# Title", "• item", "1. next", "│ quote"]
        );
    }

    #[test]
    fn inline_markup_preserves_visible_text() {
        assert_eq!(
            render_to_strings("中文 **bold** `code` [link](https://example.com)"),
            vec!["中文 bold code link"]
        );
    }
}
