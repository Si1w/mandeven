//! Markdown renderer for assistant output.
//!
//! This module keeps the TUI-facing API intentionally small:
//! [`Engine::render_into`] turns assistant markdown into ratatui lines,
//! while [`Engine::render_into_width`] uses the same path with explicit
//! terminal-width wrapping. The implementation uses `pulldown-cmark` for
//! CommonMark/GFM block parsing, then maps parser events onto the compact
//! terminal styling used by the rest of the transcript.

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub(super) struct Engine;

impl Engine {
    pub(super) fn render_into(out: &mut Vec<Line<'_>>, text: &str) {
        let mut state = EngineState::new(out, None);
        state.render(text);
    }

    pub(super) fn render_into_width(out: &mut Vec<Line<'_>>, text: &str, width: u16) {
        let mut state = EngineState::new(out, Some(usize::from(width.max(1))));
        state.render(text);
    }
}

struct EngineState<'a, 'out> {
    out: &'out mut Vec<Line<'a>>,
    frames: Vec<BlockFrame>,
    current_initial_prefix: Vec<Span<'a>>,
    current_subsequent_prefix: Vec<Span<'a>>,
    spans: Vec<Span<'a>>,
    inline_stack: Vec<InlineKind>,
    heading: Option<usize>,
    pending_item_marker: Option<String>,
    pending_block_gap: bool,
    wrap_width: Option<usize>,
    current_line_wrappable: bool,
    code_line: Option<String>,
    table: Option<TableState>,
}

impl<'a, 'out> EngineState<'a, 'out> {
    fn new(out: &'out mut Vec<Line<'a>>, wrap_width: Option<usize>) -> Self {
        Self {
            out,
            frames: Vec::new(),
            current_initial_prefix: Vec::new(),
            current_subsequent_prefix: Vec::new(),
            spans: Vec::new(),
            inline_stack: Vec::new(),
            heading: None,
            pending_item_marker: None,
            pending_block_gap: false,
            wrap_width,
            current_line_wrappable: true,
            code_line: None,
            table: None,
        }
    }

    fn render(&mut self, text: &str) {
        let parser = Parser::new_ext(text, parser_options());

        for event in parser {
            self.handle_event(event);
        }

        self.flush_current_line();
    }

    fn handle_event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start_tag(tag),
            Event::End(tag) => self.end_tag(tag),
            Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => {
                self.push_text(&text);
            }
            Event::Code(text) => self.push_inline_code(&text),
            Event::InlineMath(text) => self.push_inline_code(&format!("${text}$")),
            Event::DisplayMath(text) => {
                self.push_code_start("math");
                self.push_code_text(&text);
                self.push_code_end();
            }
            Event::FootnoteReference(label) => self.push_text(&format!("[^{label}]")),
            Event::SoftBreak => self.push_soft_break(),
            Event::HardBreak => self.push_hard_break(),
            Event::Rule => self.push_rule(),
            Event::TaskListMarker(checked) => self.push_task_marker(checked),
        }
    }

    fn start_tag(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Heading { level, .. } => {
                self.start_heading(level);
            }
            Tag::BlockQuote(_) => {
                self.flush_current_line();
                self.frames.push(BlockFrame::Quote);
            }
            Tag::CodeBlock(kind) => self.start_code_block(kind),
            Tag::List(start) => {
                self.flush_current_line();
                self.frames.push(BlockFrame::List(ListFrame {
                    next: start,
                    marker_width: default_marker_width(start),
                }));
            }
            Tag::Item => self.start_item(),
            Tag::Table(alignments) => {
                self.flush_current_line();
                self.table = Some(TableState::new(alignments));
            }
            Tag::TableHead => {
                if let Some(table) = &mut self.table {
                    table.start_row(true);
                }
            }
            Tag::TableRow => {
                if let Some(table) = &mut self.table {
                    table.start_row(false);
                }
            }
            Tag::TableCell => {
                if let Some(table) = &mut self.table {
                    table.start_cell();
                }
            }
            Tag::Emphasis | Tag::Superscript | Tag::Subscript => {
                self.inline_stack.push(InlineKind::Emphasis);
            }
            Tag::Strong => self.inline_stack.push(InlineKind::Strong),
            Tag::Strikethrough => self.inline_stack.push(InlineKind::Strikethrough),
            Tag::Link { .. } => self.inline_stack.push(InlineKind::Link),
            Tag::Image { .. } => self.inline_stack.push(InlineKind::Image),
            Tag::Paragraph
            | Tag::HtmlBlock
            | Tag::FootnoteDefinition(_)
            | Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::MetadataBlock(_) => {}
        }
    }

    fn end_tag(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Heading(_) => {
                self.flush_current_line();
                self.heading = None;
                self.mark_block_gap();
            }
            TagEnd::BlockQuote(_) => {
                self.flush_current_line();
                self.pop_frame(|frame| matches!(frame, BlockFrame::Quote));
            }
            TagEnd::CodeBlock => self.push_code_end(),
            TagEnd::Paragraph | TagEnd::HtmlBlock => {
                self.flush_current_line();
                self.mark_block_gap();
            }
            TagEnd::List(_) => {
                self.flush_current_line();
                self.pop_frame(|frame| matches!(frame, BlockFrame::List(_)));
                self.pending_item_marker = None;
            }
            TagEnd::Item => {
                self.flush_current_line();
                self.pending_item_marker = None;
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    self.push_table(table);
                }
                self.mark_block_gap();
            }
            TagEnd::TableHead | TagEnd::TableRow => {
                if let Some(table) = &mut self.table {
                    table.finish_row();
                }
            }
            TagEnd::TableCell => {
                if let Some(table) = &mut self.table {
                    table.finish_cell();
                }
            }
            TagEnd::Emphasis | TagEnd::Superscript | TagEnd::Subscript => {
                self.pop_inline(InlineKind::Emphasis);
            }
            TagEnd::Strong => self.pop_inline(InlineKind::Strong),
            TagEnd::Strikethrough => self.pop_inline(InlineKind::Strikethrough),
            TagEnd::Link => self.pop_inline(InlineKind::Link),
            TagEnd::Image => self.pop_inline(InlineKind::Image),
            TagEnd::FootnoteDefinition
            | TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition
            | TagEnd::MetadataBlock(_) => {}
        }
    }

    fn push_text(&mut self, text: &str) {
        if let Some(table) = &mut self.table {
            table.push_text(text);
            return;
        }

        if self.code_line.is_some() {
            self.push_code_text(text);
            return;
        }

        for (i, segment) in text.split('\n').enumerate() {
            if i > 0 {
                self.push_hard_break();
            }
            if !segment.is_empty() {
                self.ensure_line();
                self.spans
                    .push(Span::styled(segment.to_string(), self.inline_style()));
            }
        }
    }

    fn push_inline_code(&mut self, text: &str) {
        if let Some(table) = &mut self.table {
            table.push_text(text);
            return;
        }

        self.ensure_line();
        self.spans
            .push(Span::styled(text.to_string(), code_style()));
    }

    fn push_soft_break(&mut self) {
        if let Some(table) = &mut self.table {
            table.push_text(" ");
            return;
        }

        self.ensure_line();
        self.spans.push(Span::styled(" ", self.inline_style()));
    }

    fn push_hard_break(&mut self) {
        if let Some(table) = &mut self.table {
            table.push_text(" ");
            return;
        }

        self.flush_current_line();
    }

    fn push_rule(&mut self) {
        self.ensure_line();
        self.spans
            .push(Span::styled("────────────────────────", dim_style()));
        self.flush_current_line();
        self.mark_block_gap();
    }

    fn start_heading(&mut self, level: HeadingLevel) {
        self.flush_current_line();
        if !self.out.is_empty() && !last_line_is_blank(self.out) {
            self.out.push(Line::raw(""));
        }
        self.pending_block_gap = false;
        self.heading = Some(heading_level(level));
        self.push_heading_prefix(level);
    }

    fn push_task_marker(&mut self, checked: bool) {
        self.pending_item_marker = Some(if checked { "☑ " } else { "☐ " }.to_string());
    }

    fn push_heading_prefix(&mut self, level: HeadingLevel) {
        self.ensure_line();
        self.spans.push(Span::styled(
            format!("{} ", "#".repeat(heading_level(level))),
            accent_style().add_modifier(Modifier::BOLD),
        ));
    }

    fn start_item(&mut self) {
        let Some(list) = self.last_list_mut() else {
            return;
        };

        let marker = match list.next {
            Some(next) => {
                list.next = Some(next + 1);
                format!("{next}. ")
            }
            None => "• ".to_string(),
        };
        list.marker_width = marker.chars().count();
        self.pending_item_marker = Some(marker);
    }

    fn start_code_block(&mut self, kind: CodeBlockKind<'_>) {
        let info = match kind {
            CodeBlockKind::Fenced(info) => first_word(&info),
            CodeBlockKind::Indented => "code".to_string(),
        };
        self.push_code_start(&info);
    }

    fn push_code_start(&mut self, info: &str) {
        self.flush_current_line();
        self.ensure_line();
        self.spans.push(Span::styled("  ╭─ ", dim_style()));
        self.spans.push(Span::styled(
            display_code_info(info),
            dim_style().add_modifier(Modifier::ITALIC),
        ));
        self.current_line_wrappable = false;
        self.flush_current_line();
        self.code_line = Some(String::new());
    }

    fn push_code_text(&mut self, text: &str) {
        let Some(mut line) = self.code_line.take() else {
            return;
        };

        for ch in text.chars() {
            if ch == '\n' {
                self.push_code_line(&line);
                line.clear();
            } else {
                line.push(ch);
            }
        }

        self.code_line = Some(line);
    }

    fn push_code_end(&mut self) {
        if let Some(line) = self.code_line.take()
            && !line.is_empty()
        {
            self.push_code_line(&line);
        }

        self.ensure_line();
        self.spans.push(Span::styled("  ╰─", dim_style()));
        self.current_line_wrappable = false;
        self.flush_current_line();
        self.mark_block_gap();
    }

    fn push_code_line(&mut self, line: &str) {
        self.ensure_line();
        self.spans.push(Span::styled("  │ ", dim_style()));
        self.spans
            .push(Span::styled(line.to_string(), code_style()));
        self.current_line_wrappable = false;
        self.flush_current_line();
    }

    fn push_table(&mut self, table: TableState) {
        let widths = table.column_widths();
        for row in table.rows {
            self.ensure_line();
            for (i, cell) in row.cells.iter().enumerate() {
                if i > 0 {
                    self.spans.push(Span::styled(" │ ", dim_style()));
                }
                let alignment = table.alignments.get(i).copied().unwrap_or(Alignment::None);
                let text = pad_cell(cell, widths.get(i).copied().unwrap_or(0), alignment);
                let style = if row.is_head {
                    Style::default().add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                self.spans.push(Span::styled(text, style));
            }
            self.current_line_wrappable = false;
            self.flush_current_line();
        }
    }

    fn ensure_line(&mut self) {
        if !self.spans.is_empty() {
            return;
        }

        if self.pending_block_gap && !self.out.is_empty() && !last_line_is_blank(self.out) {
            self.out.push(Line::raw(""));
        }
        self.pending_block_gap = false;

        let (initial_prefix, consumed_marker) = self.line_prefix(true);
        let (subsequent_prefix, _) = self.line_prefix(false);
        self.current_initial_prefix = initial_prefix;
        self.current_subsequent_prefix = subsequent_prefix;
        if consumed_marker {
            self.pending_item_marker = None;
        }
    }

    fn flush_current_line(&mut self) {
        if !self.spans.is_empty() {
            let spans = std::mem::take(&mut self.spans);
            let initial_prefix = std::mem::take(&mut self.current_initial_prefix);
            let subsequent_prefix = std::mem::take(&mut self.current_subsequent_prefix);
            if let Some(width) = self.wrap_width
                && self.current_line_wrappable
            {
                push_wrapped_line(self.out, initial_prefix, &spans, subsequent_prefix, width);
            } else {
                let mut rendered = initial_prefix;
                rendered.extend(spans);
                self.out.push(Line::from(rendered));
            }
            self.current_line_wrappable = true;
        }
    }

    fn mark_block_gap(&mut self) {
        if !self.in_list() {
            self.pending_block_gap = true;
        }
    }

    fn line_prefix(&self, include_pending_marker: bool) -> (Vec<Span<'a>>, bool) {
        let mut spans = Vec::new();
        let mut consumed_marker = false;
        let marker_list_idx = include_pending_marker
            .then(|| {
                self.pending_item_marker.as_ref().and_then(|_| {
                    self.frames
                        .iter()
                        .rposition(|frame| matches!(frame, BlockFrame::List(_)))
                })
            })
            .flatten();

        for (idx, frame) in self.frames.iter().enumerate() {
            match frame {
                BlockFrame::Quote => spans.push(Span::styled("> ", dim_style())),
                BlockFrame::List(list) => {
                    if marker_list_idx == Some(idx) {
                        if let Some(marker) = &self.pending_item_marker {
                            spans.push(Span::styled(marker.clone(), dim_style()));
                            consumed_marker = true;
                        }
                    } else {
                        spans.push(Span::raw(" ".repeat(list.marker_width)));
                    }
                }
            }
        }

        (spans, consumed_marker)
    }

    fn inline_style(&self) -> Style {
        let mut style = self.base_style();
        for kind in &self.inline_stack {
            style = match kind {
                InlineKind::Emphasis | InlineKind::Image => style.add_modifier(Modifier::ITALIC),
                InlineKind::Strong => style.add_modifier(Modifier::BOLD),
                InlineKind::Strikethrough => style.add_modifier(Modifier::CROSSED_OUT),
                InlineKind::Link => Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::UNDERLINED),
            };
        }
        style
    }

    fn base_style(&self) -> Style {
        if let Some(level) = self.heading {
            heading_style(level)
        } else if self.in_quote() {
            dim_style()
        } else {
            Style::default()
        }
    }

    fn pop_inline(&mut self, kind: InlineKind) {
        if let Some(pos) = self.inline_stack.iter().rposition(|active| *active == kind) {
            self.inline_stack.remove(pos);
        }
    }

    fn pop_frame(&mut self, matches: impl Fn(&BlockFrame) -> bool) {
        if let Some(pos) = self.frames.iter().rposition(matches) {
            self.frames.remove(pos);
        }
    }

    fn last_list_mut(&mut self) -> Option<&mut ListFrame> {
        self.frames.iter_mut().rev().find_map(|frame| match frame {
            BlockFrame::List(list) => Some(list),
            BlockFrame::Quote => None,
        })
    }

    fn in_list(&self) -> bool {
        self.frames
            .iter()
            .any(|frame| matches!(frame, BlockFrame::List(_)))
    }

    fn in_quote(&self) -> bool {
        self.frames
            .iter()
            .any(|frame| matches!(frame, BlockFrame::Quote))
    }
}

#[derive(Clone, Debug)]
enum BlockFrame {
    Quote,
    List(ListFrame),
}

#[derive(Clone, Debug)]
struct ListFrame {
    next: Option<u64>,
    marker_width: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InlineKind {
    Emphasis,
    Strong,
    Strikethrough,
    Link,
    Image,
}

#[derive(Debug)]
struct TableState {
    alignments: Vec<Alignment>,
    rows: Vec<TableRow>,
    current_row: Option<TableRow>,
    current_cell: Option<String>,
}

impl TableState {
    fn new(alignments: Vec<Alignment>) -> Self {
        Self {
            alignments,
            rows: Vec::new(),
            current_row: None,
            current_cell: None,
        }
    }

    fn start_row(&mut self, is_head: bool) {
        self.finish_row();
        self.current_row = Some(TableRow {
            is_head,
            cells: Vec::new(),
        });
    }

    fn finish_row(&mut self) {
        self.finish_cell();
        if let Some(row) = self.current_row.take()
            && !row.cells.is_empty()
        {
            self.rows.push(row);
        }
    }

    fn start_cell(&mut self) {
        self.finish_cell();
        if self.current_row.is_none() {
            self.start_row(false);
        }
        self.current_cell = Some(String::new());
    }

    fn finish_cell(&mut self) {
        if let Some(cell) = self.current_cell.take()
            && let Some(row) = &mut self.current_row
        {
            row.cells.push(cell.trim().to_string());
        }
    }

    fn push_text(&mut self, text: &str) {
        if self.current_cell.is_none() {
            self.start_cell();
        }
        if let Some(cell) = &mut self.current_cell {
            cell.push_str(text);
        }
    }

    fn column_widths(&self) -> Vec<usize> {
        let columns = self
            .rows
            .iter()
            .map(|row| row.cells.len())
            .max()
            .unwrap_or(0);
        let mut widths = vec![0usize; columns];
        for row in &self.rows {
            for (i, cell) in row.cells.iter().enumerate() {
                widths[i] = widths[i].max(display_width(cell));
            }
        }
        widths
    }
}

#[derive(Debug)]
struct TableRow {
    is_head: bool,
    cells: Vec<String>,
}

fn parser_options() -> Options {
    Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS
}

fn heading_level(level: HeadingLevel) -> usize {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

fn default_marker_width(start: Option<u64>) -> usize {
    match start {
        Some(next) => format!("{next}. ").chars().count(),
        None => 2,
    }
}

fn first_word(info: &str) -> String {
    info.split_whitespace()
        .next()
        .filter(|word| !word.is_empty())
        .unwrap_or("code")
        .to_string()
}

fn display_code_info(info: &str) -> String {
    if info.is_empty() {
        "code".to_string()
    } else {
        info.to_string()
    }
}

fn pad_cell(text: &str, width: usize, alignment: Alignment) -> String {
    let pad = width.saturating_sub(display_width(text));
    match alignment {
        Alignment::Right => format!("{}{}", " ".repeat(pad), text),
        Alignment::Center => {
            let left = pad / 2;
            let right = pad - left;
            format!("{}{}{}", " ".repeat(left), text, " ".repeat(right))
        }
        Alignment::Left | Alignment::None => format!("{}{}", text, " ".repeat(pad)),
    }
}

fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

fn last_line_is_blank(lines: &[Line<'_>]) -> bool {
    lines
        .last()
        .is_some_and(|line| line.spans.iter().all(|span| span.content.trim().is_empty()))
}

pub(super) fn push_wrapped_line<'a>(
    out: &mut Vec<Line<'a>>,
    initial_prefix: Vec<Span<'a>>,
    content: &[Span<'a>],
    subsequent_prefix: Vec<Span<'a>>,
    width: usize,
) {
    let mut wrapper = LineWrapper::new(out, initial_prefix, subsequent_prefix, width.max(1));
    for span in content {
        wrapper.push_span(span);
    }
    wrapper.finish();
}

struct LineWrapper<'a, 'out> {
    out: &'out mut Vec<Line<'a>>,
    current: Vec<Span<'a>>,
    current_width: usize,
    content_width: usize,
    subsequent_prefix: Vec<Span<'a>>,
    subsequent_prefix_width: usize,
    width: usize,
}

impl<'a, 'out> LineWrapper<'a, 'out> {
    fn new(
        out: &'out mut Vec<Line<'a>>,
        initial_prefix: Vec<Span<'a>>,
        subsequent_prefix: Vec<Span<'a>>,
        width: usize,
    ) -> Self {
        let current_width = spans_width(&initial_prefix);
        let subsequent_prefix_width = spans_width(&subsequent_prefix);
        Self {
            out,
            current: initial_prefix,
            current_width,
            content_width: 0,
            subsequent_prefix,
            subsequent_prefix_width,
            width,
        }
    }

    fn push_span(&mut self, span: &Span<'a>) {
        let style = span.style;
        for token in split_wrap_tokens(span.content.as_ref()) {
            if token.is_whitespace {
                self.push_whitespace(token.text, style);
            } else {
                self.push_word(token.text, style);
            }
        }
    }

    fn push_whitespace(&mut self, text: &str, style: Style) {
        if self.content_width == 0 {
            return;
        }

        let token_width = display_width(text);
        if self.current_width.saturating_add(token_width) > self.width {
            self.break_line();
            return;
        }

        self.push_piece(text, style, token_width);
    }

    fn push_word(&mut self, text: &str, style: Style) {
        let token_width = display_width(text);
        if self.current_width.saturating_add(token_width) <= self.width {
            self.push_piece(text, style, token_width);
            return;
        }

        if self.content_width > 0 {
            self.break_line();
        }

        if self.current_width.saturating_add(token_width) <= self.width {
            self.push_piece(text, style, token_width);
            return;
        }

        let mut piece = String::new();
        let mut piece_width = 0usize;
        for ch in text.chars() {
            let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
            if piece_width > 0
                && self
                    .current_width
                    .saturating_add(piece_width)
                    .saturating_add(ch_width)
                    > self.width
            {
                self.push_piece(&piece, style, piece_width);
                piece.clear();
                piece_width = 0;
                self.break_line();
            } else if piece_width == 0
                && self.content_width > 0
                && self.current_width.saturating_add(ch_width) > self.width
            {
                self.break_line();
            }

            piece.push(ch);
            piece_width += ch_width;
        }

        if !piece.is_empty() {
            self.push_piece(&piece, style, piece_width);
        }
    }

    fn push_piece(&mut self, text: &str, style: Style, width: usize) {
        self.current.push(Span::styled(text.to_string(), style));
        self.current_width += width;
        self.content_width += width;
    }

    fn break_line(&mut self) {
        if self.content_width > 0 {
            trim_trailing_whitespace(&mut self.current);
        }
        self.out.push(Line::from(std::mem::take(&mut self.current)));
        self.current = self.subsequent_prefix.clone();
        self.current_width = self.subsequent_prefix_width;
        self.content_width = 0;
    }

    fn finish(mut self) {
        if self.content_width > 0 {
            trim_trailing_whitespace(&mut self.current);
        }
        self.out.push(Line::from(self.current));
    }
}

struct WrapToken<'a> {
    text: &'a str,
    is_whitespace: bool,
}

fn split_wrap_tokens(text: &str) -> Vec<WrapToken<'_>> {
    let mut tokens = Vec::new();
    let mut start = 0usize;
    let mut current_run_is_space = None;

    for (idx, ch) in text.char_indices() {
        let char_is_space = ch.is_whitespace();
        match current_run_is_space {
            Some(current) if current == char_is_space => {}
            Some(current) => {
                tokens.push(WrapToken {
                    text: &text[start..idx],
                    is_whitespace: current,
                });
                start = idx;
                current_run_is_space = Some(char_is_space);
            }
            None => current_run_is_space = Some(char_is_space),
        }
    }

    if let Some(is_whitespace) = current_run_is_space {
        tokens.push(WrapToken {
            text: &text[start..],
            is_whitespace,
        });
    }

    tokens
}

fn spans_width(spans: &[Span<'_>]) -> usize {
    spans
        .iter()
        .map(|span| display_width(span.content.as_ref()))
        .sum()
}

fn trim_trailing_whitespace(spans: &mut Vec<Span<'_>>) {
    while let Some(last) = spans.last_mut() {
        let trimmed = last.content.trim_end();
        if trimmed.is_empty() {
            spans.pop();
        } else if trimmed.len() < last.content.len() {
            last.content = trimmed.to_string().into();
            break;
        } else {
            break;
        }
    }
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

    fn render_to_strings_width(input: &str, width: u16) -> Vec<String> {
        let mut lines = Vec::new();
        Engine::render_into_width(&mut lines, input, width);
        lines.into_iter().map(|line| line.to_string()).collect()
    }

    #[test]
    fn single_paragraph_has_no_surrounding_blank_lines() {
        assert_eq!(
            render_to_strings("今天是 2026 年 4 月 27 日。"),
            vec!["今天是 2026 年 4 月 27 日。"]
        );
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
            vec!["# Title", "", "• item", "1. next", "> quote"]
        );
    }

    #[test]
    fn heading_keeps_one_blank_line_from_previous_block() {
        assert_eq!(
            render_to_strings("Intro paragraph\n# Header\nBody paragraph"),
            vec!["Intro paragraph", "", "# Header", "", "Body paragraph"]
        );
        assert_eq!(
            render_to_strings("- list item\n# Header"),
            vec!["• list item", "", "# Header"]
        );
        assert_eq!(
            render_to_strings("> quote\n# Header"),
            vec!["> quote", "", "# Header"]
        );
    }

    #[test]
    fn inline_markup_preserves_visible_text() {
        assert_eq!(
            render_to_strings("中文 **bold** `code` [link](https://example.com)"),
            vec!["中文 bold code link"]
        );
    }

    #[test]
    fn collapses_leading_blank_lines() {
        assert_eq!(render_to_strings("\n\nanswer"), vec!["answer"]);
    }

    #[test]
    fn renders_nested_lists_with_indentation_and_task_markers() {
        assert_eq!(
            render_to_strings("- parent\n  - [x] done\n  - [ ] todo\n10. tenth"),
            vec!["• parent", "  ☑ done", "  ☐ todo", "10. tenth"]
        );
    }

    #[test]
    fn renders_gfm_tables_as_aligned_rows() {
        assert_eq!(
            render_to_strings("| Name | Count |\n| :--- | ----: |\n| alpha | 3 |\n| beta | 12 |"),
            vec!["Name  │ Count", "alpha │     3", "beta  │    12",]
        );
    }

    #[test]
    fn resolves_reference_links_and_setext_headings() {
        assert_eq!(
            render_to_strings("Title\n=====\nSee [docs][d].\n\n[d]: https://example.com"),
            vec!["# Title", "", "See docs."]
        );
    }

    #[test]
    fn keeps_block_html_visible() {
        assert_eq!(
            render_to_strings("<div>\ncontent\n</div>"),
            vec!["<div>", "content", "</div>"]
        );
    }

    #[test]
    fn wraps_lists_and_quotes_with_continuation_prefixes() {
        assert_eq!(
            render_to_strings_width("- first second third fourth", 14),
            vec!["• first second", "  third fourth"]
        );
        assert_eq!(
            render_to_strings_width("> block quote with content that should wrap nicely", 22),
            vec![
                "> block quote with",
                "> content that should",
                "> wrap nicely"
            ]
        );
        assert_eq!(
            render_to_strings_width("1. ordered item contains many words for wrapping", 18).len(),
            4
        );
    }
}
