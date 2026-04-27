//! Markdown renderer for assistant output.
//!
//! This module keeps the TUI-facing API intentionally small:
//! [`Engine::render_into`] turns assistant markdown into ratatui lines
//! and [`Engine::line_count`] mirrors the same rendering path for
//! scroll accounting. The implementation uses `pulldown-cmark` for
//! CommonMark/GFM block parsing, then maps parser events onto the
//! compact terminal styling used by the rest of the transcript.

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

pub(super) struct Engine;

impl Engine {
    pub(super) fn render_into(out: &mut Vec<Line<'_>>, text: &str) {
        let mut renderer = Renderer::new(out);
        renderer.render(text);
    }

    pub(super) fn line_count(text: &str) -> usize {
        let mut lines: Vec<Line<'static>> = Vec::new();
        Self::render_into(&mut lines, text);
        lines.len()
    }
}

struct Renderer<'a, 'out> {
    out: &'out mut Vec<Line<'a>>,
    frames: Vec<BlockFrame>,
    spans: Vec<Span<'a>>,
    inline_stack: Vec<InlineKind>,
    heading: Option<usize>,
    pending_item_marker: Option<String>,
    pending_block_gap: bool,
    code_line: Option<String>,
    table: Option<TableState>,
}

impl<'a, 'out> Renderer<'a, 'out> {
    fn new(out: &'out mut Vec<Line<'a>>) -> Self {
        Self {
            out,
            frames: Vec::new(),
            spans: Vec::new(),
            inline_stack: Vec::new(),
            heading: None,
            pending_item_marker: None,
            pending_block_gap: false,
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
                self.heading = Some(heading_level(level));
                self.push_heading_prefix(level);
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
        self.flush_current_line();
        self.mark_block_gap();
    }

    fn push_code_line(&mut self, line: &str) {
        self.ensure_line();
        self.spans.push(Span::styled("  │ ", dim_style()));
        self.spans
            .push(Span::styled(line.to_string(), code_style()));
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

        let (prefix, consumed_marker) = self.line_prefix();
        self.spans.extend(prefix);
        if consumed_marker {
            self.pending_item_marker = None;
        }
    }

    fn flush_current_line(&mut self) {
        if !self.spans.is_empty() {
            let spans = std::mem::take(&mut self.spans);
            self.out.push(Line::from(spans));
        }
    }

    fn mark_block_gap(&mut self) {
        if !self.in_list() {
            self.pending_block_gap = true;
        }
    }

    fn line_prefix(&self) -> (Vec<Span<'a>>, bool) {
        let mut spans = Vec::new();
        let mut consumed_marker = false;
        let marker_list_idx = self.pending_item_marker.as_ref().and_then(|_| {
            self.frames
                .iter()
                .rposition(|frame| matches!(frame, BlockFrame::List(_)))
        });

        for (idx, frame) in self.frames.iter().enumerate() {
            match frame {
                BlockFrame::Quote => spans.push(Span::styled("│ ", dim_style())),
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
    text.chars().count()
}

fn last_line_is_blank(lines: &[Line<'_>]) -> bool {
    lines
        .last()
        .is_some_and(|line| line.spans.iter().all(|span| span.content.trim().is_empty()))
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
            vec!["# Title", "See docs."]
        );
    }

    #[test]
    fn keeps_block_html_visible() {
        assert_eq!(
            render_to_strings("<div>\ncontent\n</div>"),
            vec!["<div>", "content", "</div>"]
        );
    }
}
