//! Ratatui view layer — paints one frame from a [`super::CliState`].
//!
//! The layout borrows the calmer conventions used by Codex and Claude
//! Code TUIs:
//!
//! 1. Compact identity header.
//! 2. Borderless transcript that relies on whitespace and message
//!    prefixes instead of a full frame.
//! 3. One-line status strip above the composer.
//! 4. Bottom composer with top/bottom rounded rules, open sides, and
//!    dim footer hints.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};

use super::{CliState, Line as TranscriptLine, Mode, Overlay};

mod markdown;

const PROMPT: &str = "› ";
const HELP_BODY_LINES: u16 = 21;
const HELP_LABEL_WIDTH: usize = 28;
const QUEUED_PREVIEW_LIMIT: usize = 3;

const BRAND: Color = Color::Rgb(215, 119, 87);

/// Paint the entire frame from `state`.
///
/// Takes `&mut CliState` because [`render_transcript`] synchronises
/// `state.scroll_offset` with the render-time `max_offset` (so a
/// subsequent arrow-up from follow-mode moves relative to the current
/// bottom, not from zero).
pub fn render(f: &mut Frame<'_>, state: &mut CliState) {
    let queued_preview_height = queued_preview_height(state);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2), // identity header
            Constraint::Min(0),    // transcript
            Constraint::Length(1), // status strip
            Constraint::Length(queued_preview_height),
            Constraint::Length(3), // composer
        ])
        .split(f.area());

    render_header(f, chunks[0], state);
    render_transcript(f, chunks[1], state);
    render_status_line(f, chunks[2], state);
    render_queued_preview(f, chunks[3], state);
    render_input(f, chunks[4], state);

    match state.overlay {
        Some(Overlay::Help) => render_help_overlay(f, chunks[1], state),
        Some(Overlay::Skills) => render_skills_overlay(f, chunks[1], state),
        None => {}
    }
}

fn render_header(f: &mut Frame<'_>, area: Rect, state: &CliState) {
    if area.height == 0 {
        return;
    }

    let view = if state.follow_bottom {
        "live"
    } else {
        "history"
    };

    let mut lines = vec![Line::from(vec![
        Span::raw(" "),
        Span::styled("Mandeven", brand_style()),
        Span::styled("  local agent", dim_style()),
        Span::styled("  ·  ", dim_style()),
        Span::styled(view, dim_style()),
    ])];

    if area.height > 1 {
        lines.push(Line::from(vec![
            Span::raw(" "),
            Span::styled("/help", accent_style()),
            Span::styled(" commands   ", dim_style()),
            Span::styled("↑/↓", accent_style()),
            Span::styled(" transcript", dim_style()),
        ]));
    }

    f.render_widget(Paragraph::new(Text::from(lines)), area);
}

fn render_transcript(f: &mut Frame<'_>, area: Rect, state: &mut CliState) {
    if area.height == 0 {
        return;
    }

    let inner = area.inner(Margin {
        horizontal: 1,
        vertical: 0,
    });

    // Estimate logical lines ignoring terminal wrapping. At 80+
    // columns most messages fit on one line; under-counts only when
    // content actually wraps, in which case the last wrapped rows
    // may scroll out of view. Acceptable trade-off vs. pulling in
    // ratatui's unstable `Paragraph::line_count` (ratatui issue #293).
    //
    // TODO(wrap-aware-scroll): replace with `Paragraph::line_count`
    // once it stabilises, or track wrapping ourselves via
    // `unicode-width`.
    let logical = count_logical_lines(state);
    let max_offset =
        u16::try_from(logical.saturating_sub(inner.height as usize)).unwrap_or(u16::MAX);

    // follow_bottom=true -> render at the live bottom and sync
    // scroll_offset to max_offset so a subsequent arrow-up starts from
    // the current bottom view. follow_bottom=false -> render at the
    // user-frozen offset, clamped to valid range; if arrow-down pushed
    // offset past max_offset, the clamp snaps it back and re-enters
    // follow mode automatically.
    let scroll = if state.follow_bottom {
        state.scroll_offset = max_offset;
        max_offset
    } else {
        let clamped = state.scroll_offset.min(max_offset);
        state.scroll_offset = clamped;
        if clamped == max_offset {
            state.follow_bottom = true;
        }
        clamped
    };

    if has_transcript_content(state) {
        f.render_widget(
            Paragraph::new(build_transcript(state))
                .wrap(Wrap { trim: false })
                .scroll((scroll, 0)),
            inner,
        );
    } else {
        render_empty_transcript(f, inner);
    }
}

fn render_empty_transcript(f: &mut Frame<'_>, area: Rect) {
    let text = Text::from(vec![
        Line::from(vec![
            Span::styled("› ", prompt_style()),
            Span::styled("Ask Mandeven to do anything", dim_style()),
        ]),
        Line::raw(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled("Start with a prompt below, or type ", dim_style()),
            Span::styled("/help", accent_style()),
            Span::styled(" for commands.", dim_style()),
        ]),
    ]);

    f.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), area);
}

fn has_transcript_content(state: &CliState) -> bool {
    state
        .transcript
        .iter()
        .any(|entry| should_render_transcript_entry(state, entry))
        || (state.show_thinking && state.streaming_thinking.is_some())
        || state.streaming.is_some()
}

/// Count the number of logical lines that [`build_transcript`] will
/// produce, without accounting for wrapping. Mirrors the same rules
/// (inter-entry blank lines + Markdown-rendered assistant output).
fn count_logical_lines(state: &CliState) -> usize {
    let mut count = 0usize;
    let mut rendered_any = false;
    for entry in state
        .transcript
        .iter()
        .filter(|entry| should_render_transcript_entry(state, entry))
    {
        if rendered_any {
            count += 1; // blank separator
        }
        count += match entry {
            TranscriptLine::User(t)
            | TranscriptLine::Error(t)
            | TranscriptLine::Thinking(t)
            | TranscriptLine::Compact(t) => t.matches('\n').count() + 1,
            TranscriptLine::Assistant(t) => markdown::Engine::line_count(t),
        };
        rendered_any = true;
    }
    if state.show_thinking
        && let Some(thinking) = &state.streaming_thinking
    {
        if rendered_any {
            count += 1;
        }
        count += thinking.matches('\n').count() + 1;
        rendered_any = true;
    }
    if let Some(stream) = &state.streaming {
        if rendered_any {
            count += 1;
        }
        count += markdown::Engine::line_count(stream);
    }
    count
}

fn render_status_line(f: &mut Frame<'_>, area: Rect, state: &CliState) {
    let (dot, label) = match state.mode {
        Mode::Idle => (Color::Green, "Ready"),
        Mode::Replying => (Color::Yellow, "Thinking"),
    };
    let detail = if state.overlay.is_some() {
        "Esc dismiss overlay".to_string()
    } else if !state.follow_bottom {
        "history view · ↓ to latest".to_string()
    } else if !state.queued_inputs.is_empty() {
        format!("queued {} · Enter keeps queuing", state.queued_inputs.len())
    } else if state.mode == Mode::Replying {
        "Enter queues · Ctrl+C clears draft".to_string()
    } else {
        "Enter to send".to_string()
    };

    let mut spans = vec![
        Span::raw(" "),
        Span::styled("●", Style::default().fg(dot)),
        Span::raw(" "),
        Span::styled(label, Style::default().fg(dot).add_modifier(Modifier::BOLD)),
    ];
    if area.width > 28 {
        spans.extend([
            Span::styled("  ", dim_style()),
            Span::styled(detail, dim_style()),
        ]);
    }

    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn queued_preview_height(state: &CliState) -> u16 {
    if state.queued_inputs.is_empty() {
        return 0;
    }

    let visible = state.queued_inputs.len().min(QUEUED_PREVIEW_LIMIT);
    let overflow = usize::from(state.queued_inputs.len() > visible);
    u16::try_from(1 + visible + overflow).unwrap_or(u16::MAX)
}

fn render_queued_preview(f: &mut Frame<'_>, area: Rect, state: &CliState) {
    if area.height == 0 || state.queued_inputs.is_empty() {
        return;
    }

    let mut lines = Vec::new();
    lines.push(Line::from(vec![
        Span::raw(" "),
        Span::styled("• ", dim_style()),
        Span::styled("Queued follow-up inputs", dim_style()),
    ]));

    let available_items = usize::from(area.height.saturating_sub(1));
    let visible_items = available_items
        .min(QUEUED_PREVIEW_LIMIT)
        .min(state.queued_inputs.len());
    for text in state.queued_inputs.iter().take(visible_items) {
        lines.push(Line::from(vec![
            Span::raw(" "),
            Span::styled("  ↳ ", dim_style()),
            Span::styled(
                preview_input(text),
                dim_style().add_modifier(Modifier::ITALIC),
            ),
        ]));
    }

    if state.queued_inputs.len() > visible_items && lines.len() < usize::from(area.height) {
        let remaining = state.queued_inputs.len() - visible_items;
        lines.push(Line::from(vec![
            Span::raw(" "),
            Span::styled(format!("    … {remaining} more queued"), dim_style()),
        ]));
    }

    f.render_widget(Paragraph::new(Text::from(lines)), area);
}

fn preview_input(text: &str) -> String {
    let mut lines = text.lines();
    let first = lines.next().unwrap_or("").trim();
    if lines.next().is_some() {
        format!("{first} …")
    } else {
        first.to_string()
    }
}

fn render_input(f: &mut Frame<'_>, area: Rect, state: &CliState) {
    let block = Block::default()
        .borders(Borders::TOP | Borders::BOTTOM)
        .border_type(BorderType::Rounded)
        .border_style(dim_style())
        .title(Line::styled(" message ", dim_style()))
        .title_bottom(Line::styled(input_footer(area.width, state), dim_style()));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let splits = Layout::horizontal([
        Constraint::Length(u16::try_from(PROMPT.chars().count()).unwrap_or(2)),
        Constraint::Min(0),
    ])
    .split(inner);
    let prefix_rect = splits[0];
    let textarea_rect = splits[1];

    f.render_widget(
        Paragraph::new(Line::styled(PROMPT, prompt_style())),
        prefix_rect,
    );
    // `&TextArea` implements `Widget` directly; `.widget()` is
    // deprecated in ratatui-textarea 0.9.
    f.render_widget(&state.input, textarea_rect);

    // Anchor the terminal's real cursor at the textarea's cursor
    // position — but only when the overlay is closed (overlay
    // modals block input, so the cursor belongs there instead).
    // TextArea paints its own reversed-cell "visual" cursor in the
    // content buffer, but the OS-level IME overlay (Chinese /
    // Japanese / Korean preedit popup) uses the terminal's real
    // cursor coordinates to decide where to float; without this
    // call the cursor stays hidden and the popup lands wherever
    // the cursor was last (usually 0,0).
    if state.overlay.is_none() {
        let sc = state.input.screen_cursor();
        let cursor_x = textarea_rect
            .x
            .saturating_add(u16::try_from(sc.col).unwrap_or(u16::MAX));
        let cursor_y = textarea_rect
            .y
            .saturating_add(u16::try_from(sc.row).unwrap_or(u16::MAX));
        f.set_cursor_position((cursor_x, cursor_y));
    }
}

fn input_footer(width: u16, state: &CliState) -> &'static str {
    if state.overlay.is_some() {
        return match width {
            0..=45 => " ↑/↓ · Esc ",
            _ => " ↑/↓ scroll overlay · Esc dismiss ",
        };
    }

    match (state.mode, width) {
        (Mode::Idle, 0..=45) => " Enter send · /help ",
        (Mode::Idle, _) => " Enter send · Shift+Enter newline · /help ",
        (Mode::Replying, 0..=45) => " Thinking · Esc ",
        (Mode::Replying, _) => " Thinking · Enter queues · Ctrl+C clears draft ",
    }
}

fn render_help_overlay(f: &mut Frame<'_>, area: Rect, state: &mut CliState) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let overlay_rect = full_width_rect(area, HELP_BODY_LINES + 4);
    f.render_widget(Clear, overlay_rect);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(dim_style())
        .title(Line::styled(" help ", brand_style()))
        .title_bottom(Line::styled(
            overlay_footer(overlay_rect.width),
            dim_style(),
        ));

    let inner = block.inner(overlay_rect);
    f.render_widget(block, overlay_rect);

    let content_area = inner.inner(Margin {
        horizontal: 2,
        vertical: 1,
    });
    let text = build_help_text();
    let scroll = clamp_overlay_scroll(
        &mut state.overlay_scroll_offset,
        text.height(),
        content_area.height,
    );
    f.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        content_area,
    );
}

/// Render the skills overlay.
///
/// Shows one row per loaded skill with its name and (truncated)
/// description. Empty catalog gets a friendly placeholder rather
/// than an empty box.
fn render_skills_overlay(f: &mut Frame<'_>, area: Rect, state: &mut CliState) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    // 2 lines for title block padding + 1 header line + N skill lines.
    let body_lines: u16 = if state.skills.is_empty() {
        1
    } else {
        u16::try_from(state.skills.len() + 1).unwrap_or(u16::MAX)
    };
    let overlay_rect = full_width_rect(area, body_lines + 4);
    f.render_widget(Clear, overlay_rect);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(dim_style())
        .title(Line::styled(" skills ", brand_style()))
        .title_bottom(Line::styled(
            overlay_footer(overlay_rect.width),
            dim_style(),
        ));

    let inner = block.inner(overlay_rect);
    f.render_widget(block, overlay_rect);

    let content_area = inner.inner(Margin {
        horizontal: 2,
        vertical: 1,
    });
    let content_height = usize::from(body_lines);
    let scroll = clamp_overlay_scroll(
        &mut state.overlay_scroll_offset,
        content_height,
        content_area.height,
    );
    f.render_widget(
        Paragraph::new(build_skills_text(&state.skills))
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        content_area,
    );
}

fn clamp_overlay_scroll(offset: &mut u16, content_height: usize, visible_height: u16) -> u16 {
    let max_offset = content_height.saturating_sub(usize::from(visible_height));
    let max_offset = u16::try_from(max_offset).unwrap_or(u16::MAX);
    *offset = (*offset).min(max_offset);
    *offset
}

fn build_skills_text(skills: &[(String, String)]) -> Text<'_> {
    if skills.is_empty() {
        return Text::from(vec![Line::styled(
            "no skills loaded — drop a SKILL.md into ~/.mandeven/skills/<name>/",
            dim_style(),
        )]);
    }

    let mut lines: Vec<Line<'_>> = Vec::with_capacity(skills.len() + 1);
    lines.push(Line::styled(
        "Type /<name> to invoke. Esc to dismiss.",
        dim_style(),
    ));
    for (name, description) in skills {
        lines.push(Line::from(vec![
            Span::styled(format!("/{name}"), brand_style()),
            Span::raw("  "),
            Span::styled(description.as_str(), dim_style()),
        ]));
    }
    Text::from(lines)
}

fn overlay_footer(width: u16) -> &'static str {
    if width < 54 {
        " ↑/↓ · Esc "
    } else {
        " ↑/↓ or wheel scroll · Esc dismiss "
    }
}

fn full_width_rect(area: Rect, height: u16) -> Rect {
    let height = height.min(area.height);

    Rect {
        x: area.x,
        y: area.y + area.height.saturating_sub(height) / 2,
        width: area.width,
        height,
    }
}

fn build_transcript(state: &CliState) -> Text<'_> {
    let mut lines: Vec<Line<'_>> = Vec::new();

    for entry in state
        .transcript
        .iter()
        .filter(|entry| should_render_transcript_entry(state, entry))
    {
        if !lines.is_empty() {
            lines.push(Line::raw(""));
        }
        append_transcript_entry(&mut lines, entry);
    }

    if state.show_thinking
        && let Some(thinking) = &state.streaming_thinking
    {
        if !lines.is_empty() {
            lines.push(Line::raw(""));
        }
        append_thinking_lines(&mut lines, thinking);
    }
    if let Some(stream) = &state.streaming {
        if !lines.is_empty() {
            lines.push(Line::raw(""));
        }
        append_assistant_lines(&mut lines, stream);
    }

    Text::from(lines)
}

fn should_render_transcript_entry(state: &CliState, entry: &TranscriptLine) -> bool {
    state.show_thinking || !matches!(entry, TranscriptLine::Thinking(_))
}

fn append_transcript_entry<'a>(lines: &mut Vec<Line<'a>>, entry: &'a TranscriptLine) {
    match entry {
        TranscriptLine::User(text) => append_user_lines(lines, text),
        TranscriptLine::Assistant(text) => append_assistant_lines(lines, text),
        TranscriptLine::Thinking(text) => append_thinking_lines(lines, text),
        TranscriptLine::Compact(text) => append_compact_lines(lines, text),
        TranscriptLine::Error(text) => append_error_lines(lines, text),
    }
}

/// Compact summary boundary, rendered in Codex's `• `-prefixed info
/// style (see `agent-examples/codex/codex-rs/tui/src/history_cell.rs`'s
/// `new_info_event`). Continuation rows align under the message text.
fn append_compact_lines<'a>(lines: &mut Vec<Line<'a>>, text: &'a str) {
    for (i, line) in text.split('\n').enumerate() {
        let prefix = if i == 0 { "• " } else { "  " };
        lines.push(Line::from(vec![
            Span::styled(prefix, dim_style()),
            Span::styled(line, dim_style().add_modifier(Modifier::ITALIC)),
        ]));
    }
}

fn append_thinking_lines<'a>(lines: &mut Vec<Line<'a>>, text: &'a str) {
    for (i, line) in text.split('\n').enumerate() {
        let prefix = if i == 0 {
            "  thinking · "
        } else {
            "             "
        };
        lines.push(Line::from(vec![
            Span::styled(prefix, dim_style().add_modifier(Modifier::ITALIC)),
            Span::styled(line, dim_style().add_modifier(Modifier::ITALIC)),
        ]));
    }
}

fn append_user_lines<'a>(lines: &mut Vec<Line<'a>>, text: &'a str) {
    for (i, line) in text.split('\n').enumerate() {
        let prefix = if i == 0 { "› " } else { "  " };
        lines.push(Line::from(vec![
            Span::styled(prefix, prompt_style()),
            Span::styled(line, Style::default().add_modifier(Modifier::BOLD)),
        ]));
    }
}

fn append_assistant_lines<'a>(lines: &mut Vec<Line<'a>>, text: &'a str) {
    markdown::Engine::render_into(lines, text);
}

fn append_error_lines<'a>(lines: &mut Vec<Line<'a>>, text: &'a str) {
    for (i, line) in text.split('\n').enumerate() {
        let prefix = if i == 0 { "• " } else { "  " };
        lines.push(Line::from(vec![
            Span::styled(prefix, Style::default().fg(Color::Red)),
            Span::styled(line, Style::default().fg(Color::Red)),
        ]));
    }
}

fn build_help_text() -> Text<'static> {
    Text::from(vec![
        section_header("Commands"),
        Line::raw(""),
        help_entry("/help", "show this panel"),
        help_entry("/skills", "list loaded skills"),
        help_entry("/new", "start a fresh session"),
        help_entry("/list", "list saved sessions"),
        help_entry("/load <n>", "switch to a listed session"),
        help_entry("/switch [model]", "list or switch LLM profile"),
        help_entry("/switch default <model>", "save default LLM profile"),
        help_entry("/compact [focus]", "compact conversation history"),
        help_entry("/heartbeat", "show or control heartbeat"),
        help_entry("/cron", "list or control cron jobs"),
        help_entry("/exit", "quit"),
        Line::raw(""),
        section_header("Keys"),
        Line::raw(""),
        help_entry("Enter", "send input"),
        help_entry("Shift/Alt+Enter", "insert newline"),
        help_entry("\\ + Enter", "continue on a new line"),
        help_entry("Enter while busy", "queue follow-up input"),
        help_entry("Ctrl+C", "clear draft or dismiss overlay"),
        help_entry("Ctrl+D", "exit when draft is empty"),
        help_entry("Ctrl+L", "redraw screen"),
        help_entry("Ctrl+A/E", "start or end of line"),
        help_entry("Ctrl+U/K", "delete to start or end"),
        help_entry("Ctrl+W", "delete previous word"),
        help_entry("Ctrl+Z/Y", "undo or redo input edits"),
        help_entry("↑/↓", "scroll current panel"),
        help_entry("Mouse wheel", "scroll current panel"),
        help_entry("Esc", "clear draft or dismiss overlay"),
    ])
}

fn section_header(text: &'static str) -> Line<'static> {
    Line::styled(text, Style::default().add_modifier(Modifier::BOLD))
}

fn help_entry(key: &'static str, desc: &'static str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("  {key:<HELP_LABEL_WIDTH$}"), accent_style()),
        Span::styled(desc, dim_style()),
    ])
}

fn brand_style() -> Style {
    Style::default().fg(BRAND).add_modifier(Modifier::BOLD)
}

fn prompt_style() -> Style {
    Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD)
}

fn accent_style() -> Style {
    Style::default().fg(Color::Cyan)
}

fn dim_style() -> Style {
    Style::default().fg(Color::DarkGray)
}

#[cfg(test)]
mod tests {
    use super::{build_transcript, has_transcript_content};
    use crate::cli::{CliState, Line as TranscriptLine};
    use ratatui::text::Text;

    #[test]
    fn transcript_hides_thinking_when_configured() {
        let state = CliState {
            show_thinking: false,
            transcript: vec![
                TranscriptLine::User("question".to_string()),
                TranscriptLine::Thinking("hidden reasoning".to_string()),
                TranscriptLine::Assistant("answer".to_string()),
            ],
            streaming_thinking: Some("hidden streaming".to_string()),
            ..CliState::default()
        };

        let rendered = text_to_plain(&build_transcript(&state));

        assert!(rendered.contains("question"));
        assert!(rendered.contains("answer"));
        assert!(!rendered.contains("hidden reasoning"));
        assert!(!rendered.contains("hidden streaming"));
    }

    #[test]
    fn hidden_thinking_alone_is_not_visible_content() {
        let state = CliState {
            show_thinking: false,
            transcript: vec![TranscriptLine::Thinking("hidden".to_string())],
            ..CliState::default()
        };

        assert!(!has_transcript_content(&state));
    }

    fn text_to_plain(text: &Text<'_>) -> String {
        text.lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}
