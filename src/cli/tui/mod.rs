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

const LIVE_PREFIX_COLS: u16 = 2;
const PROMPT_MARKER: &str = "›";
const PROMPT_PREFIX: &str = "› ";
const CONTINUATION_PREFIX: &str = "  ";
const HELP_BODY_LINES: u16 = 27;
const HELP_LABEL_WIDTH: usize = 28;
const QUEUED_PREVIEW_LIMIT: usize = 3;

const BRAND: Color = Color::Rgb(215, 119, 87);

/// Paint the entire frame from `state`.
///
/// Takes `&mut CliState` because `render_transcript` synchronises
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

    let mut top_spans = vec![
        Span::raw(" "),
        Span::styled("Mandeven", brand_style()),
        Span::styled("  local agent", dim_style()),
        Span::styled("  ·  ", dim_style()),
        Span::styled(view, dim_style()),
    ];
    // Discord badge — visible only when the adapter is configured
    // AND its `active` flag is currently true. The watch receiver
    // is read lock-free; the background task in `CliChannel::start`
    // triggers a redraw on every transition so this stays fresh.
    let discord_active = state.discord_active.as_ref().is_some_and(|rx| *rx.borrow());
    if discord_active {
        top_spans.extend([
            Span::styled("  ·  ", dim_style()),
            Span::styled(
                "discord",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
        ]);
    }
    let mut lines = vec![Line::from(top_spans)];

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

    if inner.width == 0 {
        return;
    }

    let logical = build_transcript_for_width(state, inner.width).lines.len();
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

    let text = build_transcript_for_width(state, inner.width);
    if text.lines.is_empty() {
        render_empty_transcript(f, inner);
    } else {
        f.render_widget(Paragraph::new(text).scroll((scroll, 0)), inner);
    }
}

fn render_empty_transcript(f: &mut Frame<'_>, area: Rect) {
    let text = Text::from(vec![
        Line::from(vec![
            Span::styled(PROMPT_PREFIX, prompt_style()),
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

#[cfg(test)]
fn has_transcript_content(state: &CliState) -> bool {
    state
        .transcript
        .iter()
        .any(|entry| should_render_transcript_entry(state, entry))
        || (state.show_thinking && state.streaming_thinking.is_some())
        || state.streaming.is_some()
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

    let inner = block.inner(area).inner(Margin {
        horizontal: 1,
        vertical: 0,
    });
    f.render_widget(block, area);

    let textarea_rect = Rect {
        x: inner.x.saturating_add(LIVE_PREFIX_COLS),
        y: inner.y,
        width: inner.width.saturating_sub(LIVE_PREFIX_COLS),
        height: inner.height,
    };

    f.render_widget(
        Paragraph::new(Line::styled(PROMPT_MARKER, prompt_style())),
        Rect {
            x: inner.x,
            y: inner.y,
            width: LIVE_PREFIX_COLS.min(inner.width),
            height: inner.height,
        },
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

#[cfg(test)]
fn build_transcript(state: &CliState) -> Text<'_> {
    build_transcript_inner(state, None)
}

fn build_transcript_for_width(state: &CliState, width: u16) -> Text<'_> {
    build_transcript_inner(state, Some(width))
}

fn build_transcript_inner(state: &CliState, width: Option<u16>) -> Text<'_> {
    let mut lines: Vec<Line<'_>> = Vec::new();

    for entry in state
        .transcript
        .iter()
        .filter(|entry| should_render_transcript_entry(state, entry))
    {
        append_transcript_entry_if_visible(&mut lines, entry, width);
    }

    if state.show_thinking
        && let Some(thinking) = &state.streaming_thinking
    {
        append_rendered_lines_if_visible(&mut lines, |entry| {
            append_thinking_lines(entry, thinking, width);
        });
    }
    if let Some(stream) = &state.streaming {
        append_rendered_lines_if_visible(&mut lines, |entry| {
            append_assistant_lines(entry, stream, width);
        });
    }

    Text::from(lines)
}

fn should_render_transcript_entry(state: &CliState, entry: &TranscriptLine) -> bool {
    state.show_thinking || !matches!(entry, TranscriptLine::Thinking(_))
}

fn append_transcript_entry<'a>(
    lines: &mut Vec<Line<'a>>,
    entry: &'a TranscriptLine,
    width: Option<u16>,
) {
    match entry {
        TranscriptLine::User(text) => append_user_lines(lines, text, width),
        TranscriptLine::Assistant(text) => append_assistant_lines(lines, text, width),
        TranscriptLine::Thinking(text) => append_thinking_lines(lines, text, width),
        TranscriptLine::Compact(text) => append_compact_lines(lines, text, width),
        TranscriptLine::Error(text) => append_error_lines(lines, text, width),
    }
}

fn append_transcript_entry_if_visible<'a>(
    lines: &mut Vec<Line<'a>>,
    entry: &'a TranscriptLine,
    width: Option<u16>,
) {
    append_rendered_lines_if_visible(lines, |entry_lines| {
        append_transcript_entry(entry_lines, entry, width);
    });
}

fn append_rendered_lines_if_visible<'a>(
    lines: &mut Vec<Line<'a>>,
    render: impl FnOnce(&mut Vec<Line<'a>>),
) {
    let mut entry_lines = Vec::new();
    render(&mut entry_lines);
    if entry_lines.is_empty() {
        return;
    }
    if !lines.is_empty() {
        lines.push(Line::raw(""));
    }
    lines.extend(entry_lines);
}

/// Compact summary boundary, rendered in Codex's `• `-prefixed info
/// style (see `agent-examples/codex/codex-rs/tui/src/history_cell.rs`'s
/// `new_info_event`). Continuation rows align under the message text.
fn append_compact_lines<'a>(lines: &mut Vec<Line<'a>>, text: &'a str, width: Option<u16>) {
    for (i, line) in text.split('\n').enumerate() {
        let prefix = if i == 0 { "• " } else { "  " };
        append_prefixed_line(
            lines,
            prefix,
            CONTINUATION_PREFIX,
            dim_style(),
            line,
            dim_style().add_modifier(Modifier::ITALIC),
            width,
        );
    }
}

fn append_thinking_lines<'a>(lines: &mut Vec<Line<'a>>, text: &'a str, width: Option<u16>) {
    for (i, line) in text.split('\n').enumerate() {
        let prefix = if i == 0 {
            "  thinking · "
        } else {
            "             "
        };
        append_prefixed_line(
            lines,
            prefix,
            "             ",
            dim_style().add_modifier(Modifier::ITALIC),
            line,
            dim_style().add_modifier(Modifier::ITALIC),
            width,
        );
    }
}

fn append_user_lines<'a>(lines: &mut Vec<Line<'a>>, text: &'a str, width: Option<u16>) {
    for (i, line) in text.split('\n').enumerate() {
        let prefix = if i == 0 {
            PROMPT_PREFIX
        } else {
            CONTINUATION_PREFIX
        };
        append_prefixed_line(
            lines,
            prefix,
            CONTINUATION_PREFIX,
            prompt_style(),
            line,
            Style::default().add_modifier(Modifier::BOLD),
            width,
        );
    }
}

fn append_assistant_lines<'a>(lines: &mut Vec<Line<'a>>, text: &'a str, width: Option<u16>) {
    if let Some(width) = width {
        markdown::Engine::render_into_width(lines, text, width);
    } else {
        markdown::Engine::render_into(lines, text);
    }
}

fn append_error_lines<'a>(lines: &mut Vec<Line<'a>>, text: &'a str, width: Option<u16>) {
    for (i, line) in text.split('\n').enumerate() {
        let prefix = if i == 0 { "• " } else { "  " };
        append_prefixed_line(
            lines,
            prefix,
            CONTINUATION_PREFIX,
            Style::default().fg(Color::Red),
            line,
            Style::default().fg(Color::Red),
            width,
        );
    }
}

fn append_prefixed_line<'a>(
    lines: &mut Vec<Line<'a>>,
    initial_prefix: &'static str,
    subsequent_prefix: &'static str,
    prefix_style: Style,
    text: &'a str,
    text_style: Style,
    width: Option<u16>,
) {
    let initial_prefix = Span::styled(initial_prefix, prefix_style);
    let content = Span::styled(text, text_style);
    let subsequent_prefix = Span::styled(subsequent_prefix, prefix_style);
    if let Some(width) = width {
        let content = [content];
        markdown::push_wrapped_line(
            lines,
            vec![initial_prefix],
            &content,
            vec![subsequent_prefix],
            usize::from(width.max(1)),
        );
    } else {
        lines.push(Line::from(vec![initial_prefix, content]));
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
        help_entry("/discord", "toggle Discord gateway connection"),
        help_entry("/discord status", "show enabled/disabled + allow count"),
        help_entry("/discord list", "show Discord allow list"),
        help_entry("/discord allow <id>", "add user id to allow list"),
        help_entry("/discord deny <id>", "remove user id from allow list"),
        help_entry(
            "/discord autostart on|off",
            "persist boot-time enabled flag",
        ),
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
    use super::{build_transcript, build_transcript_for_width, has_transcript_content, render};
    use crate::cli::{CliState, Line as TranscriptLine};
    use ratatui::text::Text;
    use ratatui::{Terminal, backend::TestBackend, buffer::Buffer};

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

    /// Reply text that opens with leading newlines must not stack
    /// extra blank lines on top of the entry separator that
    /// [`build_transcript`] already inserts. The markdown parser now
    /// collapses leading blank lines before rendering visible blocks.
    #[test]
    fn leading_newlines_in_assistant_reply_do_not_stack_blank_lines() {
        let state = CliState {
            transcript: vec![
                TranscriptLine::User("question".to_string()),
                TranscriptLine::Assistant("\n\n\nactual answer".to_string()),
            ],
            ..CliState::default()
        };

        let rendered = text_to_plain(&build_transcript(&state));
        let lines: Vec<&str> = rendered.split('\n').collect();

        let user_idx = lines.iter().position(|l| l.contains("question")).unwrap();
        let answer_idx = lines
            .iter()
            .position(|l| l.contains("actual answer"))
            .unwrap();

        // Exactly one blank line between user and assistant —
        // the entry separator, no leading-newline pile-up.
        assert_eq!(
            answer_idx - user_idx,
            2,
            "expected user and answer separated by exactly one blank line, got: {lines:?}"
        );
    }

    #[test]
    fn whitespace_only_streaming_assistant_does_not_add_separator() {
        let state = CliState {
            transcript: vec![TranscriptLine::User("今天是几号啊".to_string())],
            streaming: Some("\n\n".to_string()),
            ..CliState::default()
        };

        assert_eq!(text_to_plain(&build_transcript(&state)), "› 今天是几号啊");
    }

    #[test]
    fn date_answer_with_leading_newlines_uses_one_separator() {
        let state = CliState {
            transcript: vec![
                TranscriptLine::User("今天是几号啊".to_string()),
                TranscriptLine::Assistant("\n\n今天是 2026 年 4 月 27 日。".to_string()),
            ],
            ..CliState::default()
        };

        assert_eq!(
            text_to_plain(&build_transcript(&state)),
            "› 今天是几号啊\n\n今天是 2026 年 4 月 27 日。"
        );
    }

    #[test]
    fn composer_prompt_aligns_with_transcript_user_prompt() {
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = CliState {
            transcript: vec![TranscriptLine::User("query".to_string())],
            ..CliState::default()
        };
        state.input.insert_str("draft");

        terminal.draw(|f| render(f, &mut state)).unwrap();
        let buffer = terminal.backend().buffer();
        let transcript_line = buffer_line(buffer, 2);
        let input_line = buffer_line(buffer, 8);

        assert_eq!(transcript_line.find('›'), input_line.find('›'));
        assert_eq!(transcript_line.find("query"), input_line.find("draft"));
    }

    #[test]
    fn user_transcript_wraps_under_prompt_gutter() {
        let state = CliState {
            transcript: vec![TranscriptLine::User(
                "one two three four five six seven".to_string(),
            )],
            ..CliState::default()
        };

        assert_eq!(
            text_to_plain(&build_transcript_for_width(&state, 14)),
            "› one two\n  three four\n  five six\n  seven"
        );
    }

    #[test]
    fn assistant_markdown_wraps_structural_prefixes() {
        let state = CliState {
            transcript: vec![TranscriptLine::Assistant(
                "# Header\nBody paragraph\n\n> quoted content that wraps".to_string(),
            )],
            ..CliState::default()
        };

        assert_eq!(
            text_to_plain(&build_transcript_for_width(&state, 20)),
            "# Header\n\nBody paragraph\n\n> quoted content\n> that wraps"
        );
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

    fn buffer_line(buffer: &Buffer, y: u16) -> String {
        (0..buffer.area.width)
            .map(|x| buffer.cell((x, y)).unwrap().symbol())
            .collect()
    }
}
