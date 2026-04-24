//! Ratatui view layer — paints one frame from a [`super::CliState`].
//!
//! Layout (vertical):
//!
//! 1. Conversation panel (flex, rounded border, `Conversation` title,
//!    auto-scrolls to bottom via [`Paragraph::line_count`]).
//! 2. Status bar (1 row, no border — colored `●` + label).
//! 3. Input panel (fixed 3 rows, rounded border, no top title, footer
//!    hint in the bottom title).
//!
//! When the help overlay is active, it is rendered over the
//! conversation rect: full-width, bottom-anchored so the overlay's
//! bottom border overwrites the conversation's bottom border.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};

use super::{CliState, Line as TranscriptLine, Mode, Overlay};

/// Visual prefix rendered to the left of the textarea inside the
/// input panel.
const PROMPT: &str = "> ";

/// Footer hint shown in the input panel's bottom border title.
const FOOTER_HINT: &str = " type / for commands · [Esc]interrupt ";

/// Total content lines inside the help overlay (excluding borders +
/// internal padding). Updating the help body text requires updating
/// this constant.
const HELP_BODY_LINES: u16 = 11;

/// Paint the entire frame from `state`.
///
/// Takes `&mut CliState` because [`render_conversation`] synchronises
/// `state.scroll_offset` with the render-time `max_offset` (so a
/// subsequent `PgUp` from follow-mode moves relative to the current
/// bottom, not from zero).
pub fn render(f: &mut Frame<'_>, state: &mut CliState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),    // conversation (flex)
            Constraint::Length(1), // status bar
            Constraint::Length(3), // input panel
        ])
        .split(f.area());

    render_conversation(f, chunks[0], state);
    render_status_bar(f, chunks[1], state);
    render_input(f, chunks[2], state);

    if matches!(state.overlay, Some(Overlay::Help)) {
        render_help_overlay(f, chunks[0]);
    }
}

fn render_conversation(f: &mut Frame<'_>, area: Rect, state: &mut CliState) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded);

    let inner = block.inner(area);

    // Scroll math first — it only reads line counts (no string
    // borrows) and mutates state, so it must finish before we take a
    // shared borrow to build `text`.
    //
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

    // follow_bottom=true → render at the live bottom and sync
    // scroll_offset to max_offset so a subsequent PgUp starts from
    // the current bottom view. follow_bottom=false → render at the
    // user-frozen offset, clamped to valid range; if a PgDn pushed
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

    let text = build_transcript(state);

    f.render_widget(block, area);
    f.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        inner,
    );
}

/// Count the number of logical lines that [`build_transcript`] will
/// produce, without accounting for wrapping. Mirrors the same rules
/// (inter-entry blank lines + per-`\n` line split).
fn count_logical_lines(state: &CliState) -> usize {
    let mut count = 0usize;
    for (i, entry) in state.transcript.iter().enumerate() {
        if i > 0 {
            count += 1; // blank separator
        }
        count += match entry {
            TranscriptLine::User(t) | TranscriptLine::Assistant(t) | TranscriptLine::Error(t) => {
                t.matches('\n').count() + 1
            }
        };
    }
    if let Some(stream) = &state.streaming {
        if !state.transcript.is_empty() {
            count += 1;
        }
        count += stream.matches('\n').count() + 1;
    }
    count
}

fn render_status_bar(f: &mut Frame<'_>, area: Rect, state: &CliState) {
    let (dot_color, label) = match state.mode {
        Mode::Idle => (Color::Green, "Ready"),
        Mode::Replying => (Color::Yellow, "Thinking..."),
    };
    let line = Line::from(vec![
        Span::raw(" "),
        Span::styled("●", Style::default().fg(dot_color)),
        Span::raw(" "),
        Span::styled(label, Style::default().fg(Color::DarkGray)),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

fn render_input(f: &mut Frame<'_>, area: Rect, state: &CliState) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title_bottom(Line::styled(
            FOOTER_HINT,
            Style::default().fg(Color::DarkGray),
        ));

    let inner = block.inner(area);
    f.render_widget(block, area);

    // Split the input line into: `> ` prefix (cyan) + TextArea widget.
    let splits = Layout::horizontal([
        Constraint::Length(u16::try_from(PROMPT.chars().count()).unwrap_or(2)),
        Constraint::Min(0),
    ])
    .split(inner);
    let prefix_rect = splits[0];
    let textarea_rect = splits[1];

    f.render_widget(
        Paragraph::new(Line::styled(PROMPT, Style::default().fg(Color::Cyan))),
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

fn render_help_overlay(f: &mut Frame<'_>, conv_area: Rect) {
    // Height = content + 2 border + 2 vertical padding (inner margin).
    let desired_h = HELP_BODY_LINES + 4;
    let clamped_h = desired_h.min(conv_area.height);

    // Full-width, bottom-anchored: overlay.bottom == conv_area.bottom,
    // so overlay's bottom border row overwrites the conversation's
    // bottom border in the same span.
    let overlay_rect = Rect {
        x: conv_area.x,
        y: conv_area.y + conv_area.height - clamped_h,
        width: conv_area.width,
        height: clamped_h,
    };

    f.render_widget(Clear, overlay_rect);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(Line::styled(
            " Help ",
            Style::default().add_modifier(Modifier::BOLD),
        ))
        .title_bottom(Line::styled(
            " [Esc] dismiss ",
            Style::default().fg(Color::DarkGray),
        ));

    let inner = block.inner(overlay_rect);
    f.render_widget(block, overlay_rect);

    let content_area = inner.inner(Margin {
        horizontal: 2,
        vertical: 1,
    });
    f.render_widget(
        Paragraph::new(build_help_text()).wrap(Wrap { trim: false }),
        content_area,
    );
}

fn build_transcript(state: &CliState) -> Text<'_> {
    let mut lines: Vec<Line<'_>> = Vec::new();

    for (i, entry) in state.transcript.iter().enumerate() {
        if i > 0 {
            lines.push(Line::raw(""));
        }
        append_transcript_entry(&mut lines, entry);
    }

    if let Some(stream) = &state.streaming {
        if !state.transcript.is_empty() {
            lines.push(Line::raw(""));
        }
        for l in stream.split('\n') {
            lines.push(Line::raw(l));
        }
    }

    Text::from(lines)
}

fn append_transcript_entry<'a>(lines: &mut Vec<Line<'a>>, entry: &'a TranscriptLine) {
    match entry {
        TranscriptLine::User(text) => {
            lines.push(Line::from(vec![
                Span::styled("> ", Style::default().fg(Color::Cyan)),
                Span::raw(text.as_str()),
            ]));
        }
        TranscriptLine::Assistant(text) => {
            for l in text.split('\n') {
                lines.push(Line::raw(l));
            }
        }
        TranscriptLine::Error(text) => {
            for l in text.split('\n') {
                lines.push(Line::styled(l, Style::default().fg(Color::Red)));
            }
        }
    }
}

fn build_help_text() -> Text<'static> {
    Text::from(vec![
        Line::styled("COMMANDS", Style::default().add_modifier(Modifier::BOLD)),
        Line::raw(""),
        help_entry("/help", "      show this"),
        help_entry("/exit", "      quit"),
        Line::raw(""),
        Line::styled("KEYS", Style::default().add_modifier(Modifier::BOLD)),
        Line::raw(""),
        help_entry("Enter", "      send input"),
        help_entry("Backspace", "  delete char"),
        help_entry("Esc", "        interrupt · dismiss overlay"),
        help_entry("Ctrl-D", "     emergency exit"),
    ])
}

fn help_entry(key: &'static str, desc: &'static str) -> Line<'static> {
    Line::from(vec![
        Span::raw("    "),
        Span::styled(key, Style::default().fg(Color::Cyan)),
        Span::raw(desc),
    ])
}
