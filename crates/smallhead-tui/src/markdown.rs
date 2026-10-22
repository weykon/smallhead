//! Markdown presentation only; stored messages and diagnostic payloads stay verbatim.
use std::collections::VecDeque;

use pulldown_cmark::{Alignment, CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const CACHE_BYTES: usize = 2 * 1024 * 1024;
const CACHE_ENTRIES: usize = 64;

#[derive(Default)]
pub struct MarkdownCache {
    entries: VecDeque<Cached>,
    bytes: usize,
}

struct Cached {
    source: String,
    width: usize,
    lines: Vec<Line<'static>>,
    bytes: usize,
}

impl MarkdownCache {
    pub fn render(&mut self, source: &str, width: usize) -> Vec<Line<'static>> {
        let width = width.max(1);
        if let Some(index) = self
            .entries
            .iter()
            .position(|e| e.width == width && e.source == source)
        {
            let entry = self.entries.remove(index).expect("matched entry");
            let lines = entry.lines.clone();
            self.entries.push_back(entry);
            return lines;
        }
        let lines = render(source, width);
        let bytes = source.len()
            + lines
                .iter()
                .map(|line| {
                    std::mem::size_of::<Line>()
                        + line
                            .spans
                            .iter()
                            .map(|span| std::mem::size_of::<Span>() + span.content.len())
                            .sum::<usize>()
                })
                .sum::<usize>();
        if bytes <= CACHE_BYTES {
            while self.entries.len() >= CACHE_ENTRIES || self.bytes + bytes > CACHE_BYTES {
                if let Some(old) = self.entries.pop_front() {
                    self.bytes -= old.bytes;
                }
            }
            self.bytes += bytes;
            self.entries.push_back(Cached {
                source: source.into(),
                width,
                lines: lines.clone(),
                bytes,
            });
        }
        lines
    }
}

#[derive(Default)]
struct Prefix {
    first: String,
    rest: String,
    used: bool,
}

#[derive(Default)]
struct Table {
    rows: Vec<Vec<Line<'static>>>,
    row: Vec<Line<'static>>,
    alignments: Vec<Alignment>,
}

struct Writer {
    width: usize,
    lines: Vec<Line<'static>>,
    line: Line<'static>,
    styles: Vec<Style>,
    lists: Vec<Option<u64>>,
    prefixes: Vec<Prefix>,
    links: Vec<(String, String)>,
    table: Option<Table>,
}

fn render(source: &str, width: usize) -> Vec<Line<'static>> {
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES;
    let mut w = Writer {
        width,
        lines: vec![],
        line: Line::default(),
        styles: vec![Style::default()],
        lists: vec![],
        prefixes: vec![],
        links: vec![],
        table: None,
    };
    for event in Parser::new_ext(source, options) {
        match event {
            Event::Start(tag) => w.start(tag),
            Event::End(tag) => w.end(tag),
            Event::Text(text) => {
                for (_, label) in &mut w.links {
                    label.push_str(&text);
                }
                w.text(&text);
            }
            Event::Code(text) => {
                for (_, label) in &mut w.links {
                    label.push_str(&text);
                }
                w.push_style(Style::default().fg(Color::Cyan));
                w.text(&text);
                w.styles.pop();
            }
            Event::SoftBreak => w.text(" "),
            Event::HardBreak if w.table.is_some() => w.text(" / "),
            Event::HardBreak => w.flush(true),
            Event::Rule => {
                w.space();
                w.text(&"─".repeat(width.min(40)));
                w.space();
            }
            Event::TaskListMarker(done) => w.text(if done { "[x] " } else { "[ ] " }),
            Event::FootnoteReference(label) => w.text(&format!("[{label}]")),
            // HTML is displayed literally, never interpreted as terminal commands.
            Event::Html(text) | Event::InlineHtml(text) => w.text(&text),
            Event::InlineMath(text) | Event::DisplayMath(text) => w.text(&text),
        }
    }
    w.flush(false);
    while w.lines.last().is_some_and(|line| line.spans.is_empty()) {
        w.lines.pop();
    }
    w.lines
}

impl Writer {
    fn push_style(&mut self, style: Style) {
        self.styles
            .push(self.styles.last().copied().unwrap_or_default().patch(style));
    }

    fn text(&mut self, text: &str) {
        let style = self.styles.last().copied().unwrap_or_default();
        for (index, part) in text.split('\n').enumerate() {
            if index > 0 {
                if self.table.is_some() {
                    self.text(" / ");
                } else {
                    self.flush(true);
                }
            }
            let text: String = part
                .chars()
                .flat_map(|c| {
                    if c == '\t' {
                        "    ".chars().collect::<Vec<_>>()
                    } else if c.is_control() {
                        vec![]
                    } else {
                        vec![c]
                    }
                })
                .collect();
            if !text.is_empty() {
                push_span(&mut self.line, text, style);
            }
        }
    }

    fn flush(&mut self, empty: bool) {
        if self.line.spans.is_empty() && !empty {
            return;
        }
        let line = std::mem::take(&mut self.line);
        let first: String = self
            .prefixes
            .iter()
            .map(|p| {
                if p.used {
                    p.rest.as_str()
                } else {
                    p.first.as_str()
                }
            })
            .collect();
        let rest: String = self.prefixes.iter().map(|p| p.rest.as_str()).collect();
        for p in &mut self.prefixes {
            p.used = true;
        }
        self.lines
            .extend(wrap_line(line, self.width, &first, &rest));
    }

    fn space(&mut self) {
        self.flush(false);
        if self.lines.last().is_some_and(|line| !line.spans.is_empty()) {
            self.lines.push(Line::default());
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {}
            Tag::Heading { .. } => {
                self.space();
                self.push_style(
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                );
            }
            Tag::Strong => self.push_style(Style::default().add_modifier(Modifier::BOLD)),
            Tag::Emphasis => self.push_style(Style::default().add_modifier(Modifier::ITALIC)),
            Tag::Strikethrough => {
                self.push_style(Style::default().add_modifier(Modifier::CROSSED_OUT))
            }
            Tag::BlockQuote(_) => {
                self.space();
                self.prefixes.push(Prefix {
                    first: "> ".into(),
                    rest: "> ".into(),
                    used: false,
                });
            }
            Tag::List(start) => {
                self.flush(false);
                self.lists.push(start);
            }
            Tag::Item => {
                self.flush(false);
                let marker = match self.lists.last_mut() {
                    Some(Some(index)) => {
                        let s = format!("{index}. ");
                        *index += 1;
                        s
                    }
                    _ => "• ".into(),
                };
                self.prefixes.push(Prefix {
                    rest: " ".repeat(marker.width()),
                    first: marker,
                    used: false,
                });
            }
            Tag::CodeBlock(kind) => {
                self.space();
                if let CodeBlockKind::Fenced(language) = kind {
                    if !language.is_empty() {
                        self.text(&language);
                        self.flush(false);
                    }
                }
                self.prefixes.push(Prefix {
                    first: "│ ".into(),
                    rest: "│ ".into(),
                    used: false,
                });
                self.push_style(Style::default().fg(Color::Cyan));
            }
            Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. } => {
                self.links.push((dest_url.to_string(), String::new()));
                self.push_style(
                    Style::default()
                        .fg(Color::Blue)
                        .add_modifier(Modifier::UNDERLINED),
                );
            }
            Tag::FootnoteDefinition(label) => {
                self.space();
                self.text(&format!("[{label}] "));
            }
            Tag::Table(alignments) => {
                self.space();
                self.table = Some(Table {
                    alignments,
                    ..Default::default()
                });
            }
            Tag::TableCell => {
                self.line = Line::default();
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                if self.lists.is_empty() {
                    self.space();
                } else {
                    self.flush(false);
                }
            }
            TagEnd::Heading(_) => {
                self.flush(false);
                self.styles.pop();
                self.space();
            }
            TagEnd::Strong | TagEnd::Emphasis | TagEnd::Strikethrough => {
                self.styles.pop();
            }
            TagEnd::Item => {
                self.flush(false);
                self.prefixes.pop();
            }
            TagEnd::List(_) => {
                self.flush(false);
                self.lists.pop();
                if self.lists.is_empty() {
                    self.space();
                }
            }
            TagEnd::BlockQuote(_) => {
                self.flush(false);
                self.prefixes.pop();
                self.space();
            }
            TagEnd::CodeBlock => {
                self.flush(false);
                self.prefixes.pop();
                self.styles.pop();
                self.space();
            }
            TagEnd::Link | TagEnd::Image => {
                self.styles.pop();
                if let Some((url, label)) = self.links.pop() {
                    if url != label {
                        self.text(&format!(" ({url})"));
                    }
                }
            }
            TagEnd::TableCell => {
                if let Some(table) = &mut self.table {
                    table.row.push(std::mem::take(&mut self.line));
                }
            }
            TagEnd::TableHead | TagEnd::TableRow => {
                if let Some(table) = &mut self.table {
                    table.rows.push(std::mem::take(&mut table.row));
                }
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    for line in render_table(table, self.width) {
                        self.line = line;
                        self.flush(true);
                    }
                }
                self.space();
            }
            TagEnd::HtmlBlock | TagEnd::FootnoteDefinition => self.space(),
            _ => {}
        }
    }
}

fn push_span(line: &mut Line<'static>, text: String, style: Style) {
    if let Some(last) = line.spans.last_mut().filter(|span| span.style == style) {
        last.content.to_mut().push_str(&text);
    } else {
        line.spans.push(Span::styled(text, style));
    }
}

// Wrap styled graphemes before viewport slicing, so scroll offsets count physical rows.
fn wrap_line(line: Line<'static>, width: usize, first: &str, rest: &str) -> Vec<Line<'static>> {
    let width = width.max(1);
    let prefix = |text: &str| -> String {
        let mut output = String::new();
        for g in text.graphemes(true) {
            if output.width() + g.width() >= width {
                break;
            }
            output.push_str(g);
        }
        output
    };
    let make_line = |text: &str| {
        Line::from(Span::styled(
            prefix(text),
            Style::default().fg(Color::DarkGray),
        ))
    };
    let mut current = make_line(first);
    let mut used = current.width();
    let mut content = false;
    let mut result = vec![];
    for span in line.spans {
        for g in span.content.graphemes(true) {
            if g.width() > width {
                continue;
            } // A wide glyph cannot occupy a one-cell viewport.
            if used + g.width() > width && content {
                result.push(current);
                current = make_line(rest);
                used = current.width();
            }
            if used + g.width() > width {
                current = Line::default();
                used = 0;
            }
            push_span(&mut current, g.into(), span.style);
            used += g.width();
            content = true;
        }
    }
    result.push(current);
    result
}

fn render_table(table: Table, width: usize) -> Vec<Line<'static>> {
    let columns = table.alignments.len();
    if columns == 0 {
        return vec![];
    }
    let mut sizes = vec![1; columns];
    for row in &table.rows {
        for (col, cell) in row.iter().enumerate().take(columns) {
            sizes[col] = sizes[col].max(cell.width());
        }
    }
    let mut output = vec![];
    if width < columns * 6 {
        // Narrow tables become labelled records so cell contents are never clipped.
        let headers = table.rows.first().cloned().unwrap_or_default();
        for row in table.rows.iter().skip(1) {
            for (col, cell) in row.iter().enumerate() {
                let mut line = headers.get(col).cloned().unwrap_or_default();
                for span in &mut line.spans {
                    span.style = span.style.add_modifier(Modifier::BOLD);
                }
                line.spans.push(Span::raw(": "));
                line.spans.extend(cell.spans.clone());
                output.extend(wrap_line(line, width, "", ""));
            }
            output.push(Line::default());
        }
        if table.rows.len() == 1 {
            for cell in headers {
                output.extend(wrap_line(cell, width, "", ""));
            }
        }
        return output;
    }
    let available = width.saturating_sub(3 * (columns - 1));
    while sizes.iter().sum::<usize>() > available {
        let col = sizes
            .iter()
            .enumerate()
            .max_by_key(|(_, size)| *size)
            .map(|(i, _)| i)
            .unwrap_or(0);
        sizes[col] -= 1;
    }
    for (row_index, row) in table.rows.iter().enumerate() {
        let cells: Vec<_> = (0..columns)
            .map(|col| {
                wrap_line(
                    row.get(col).cloned().unwrap_or_default(),
                    sizes[col],
                    "",
                    "",
                )
            })
            .collect();
        let height = cells.iter().map(Vec::len).max().unwrap_or(1);
        for y in 0..height {
            let mut line = Line::default();
            for col in 0..columns {
                if col > 0 {
                    line.spans
                        .push(Span::styled(" │ ", Style::default().fg(Color::DarkGray)));
                }
                let mut cell = cells[col].get(y).cloned().unwrap_or_default();
                let padding = sizes[col].saturating_sub(cell.width());
                let before = match table.alignments[col] {
                    Alignment::Right => padding,
                    Alignment::Center => padding / 2,
                    _ => 0,
                };
                line.spans.push(Span::raw(" ".repeat(before)));
                if row_index == 0 {
                    for span in &mut cell.spans {
                        span.style = span.style.add_modifier(Modifier::BOLD);
                    }
                }
                line.spans.extend(cell.spans);
                line.spans.push(Span::raw(" ".repeat(padding - before)));
            }
            output.push(line);
        }
        if row_index == 0 {
            output.push(Line::styled(
                sizes
                    .iter()
                    .map(|size| "─".repeat(*size))
                    .collect::<Vec<_>>()
                    .join("─┼─"),
                Style::default().fg(Color::DarkGray),
            ));
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(lines: &[Line<'_>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn nested_styles_and_learning_examples_render_without_markup() {
        let source = "## 学习率\n\n**重点和 *强调***，然后恢复普通。\n\n1. 输入 `x = 3`\n2. **误差**\n\n> 检查方向\n\n- [x] 已算出\n- [ ] 再检查\n\n```python\nif True:\n    gradient = 2 * 3 * 0.5\n```";
        let lines = render(source, 60);
        let text = plain(&lines);
        assert!(!text.contains("**"));
        assert!(!text.contains("```"));
        for expected in [
            "学习率",
            "1. 输入 x = 3",
            "2. 误差",
            "> 检查方向",
            "[x] 已算出",
            "[ ] 再检查",
            "    gradient = 2 * 3 * 0.5",
        ] {
            assert!(text.contains(expected), "missing {expected}: {text}");
        }
        let spans: Vec<_> = lines.iter().flat_map(|line| &line.spans).collect();
        assert!(spans.iter().any(|s| s.content.contains("强调")
            && s.style
                .add_modifier
                .contains(Modifier::BOLD | Modifier::ITALIC)));
        assert!(spans
            .iter()
            .any(|s| s.content.contains("普通") && s.style == Style::default()));
        assert!(spans
            .iter()
            .any(|s| s.content == "x = 3" && s.style.fg == Some(Color::Cyan)));
    }

    #[test]
    fn tables_keep_all_cells_at_wide_and_narrow_widths() {
        let source = "| 参数 | 数值 |\n| :--- | ---: |\n| alpha | **0.01** |\n| weight | `0.496` |";
        for width in [1, 8, 20, 70] {
            let lines = render(source, width);
            assert!(lines.iter().all(|line| line.width() <= width));
            let text = plain(&lines).split_whitespace().collect::<String>();
            for expected in ["alpha", "0.01", "weight", "0.496"] {
                assert!(text.contains(expected), "width={width}: {text}");
            }
            assert!(!text.contains("**"));
        }
    }

    #[test]
    fn unicode_links_and_partial_streams_wrap_without_losing_the_tail() {
        let source = "**中文👩‍💻重点** [原文](https://example.org/very/long/path?tail=END)\n\n```rust\n    let 权重 = 0.496;\n    // streaming";
        for width in [4, 11, 40, 80] {
            let lines = render(source, width);
            assert!(
                lines.iter().all(|line| line.width() <= width),
                "{width}: {}",
                plain(&lines)
            );
            let text = plain(&lines)
                .chars()
                .filter(|c| !c.is_whitespace() && *c != '│')
                .collect::<String>();
            assert!(text.contains("中文👩‍💻重点"));
            assert!(text.contains("?tail=END"));
            assert!(text.contains("let权重=0.496;"));
            assert!(text.ends_with("//streaming"));
        }
        for (offset, _) in source.char_indices() {
            assert!(render(&source[..offset], 12)
                .iter()
                .all(|line| line.width() <= 12));
        }
    }

    #[test]
    fn literals_and_control_characters_never_become_terminal_escapes() {
        let source = "<b>literal</b>\n\n`**literal**` \u{1b}[31m\n\n---\n\n![diagram](https://example.org/img.png)";
        let text = plain(&render(source, 80));
        assert!(text.contains("<b>literal</b>"));
        assert!(text.contains("**literal**"));
        assert!(text.contains("diagram (https://example.org/img.png)"));
        assert!(!text.contains('\u{1b}'));
    }

    #[test]
    fn cache_tracks_width_and_stream_changes_and_stays_bounded() {
        let mut cache = MarkdownCache::default();
        let first = cache.render("**first**", 20);
        assert_eq!(first, cache.render("**first**", 20));
        assert_eq!(cache.entries.len(), 1);
        assert_ne!(first, cache.render("**first**", 3));
        assert!(plain(&cache.render("**first** more", 20)).contains("more"));
        for index in 0..100 {
            cache.render(&format!("**message {index}**"), 20);
        }
        assert!(cache.entries.len() <= CACHE_ENTRIES);
        assert!(cache.bytes <= CACHE_BYTES);
        let large = "x".repeat(CACHE_BYTES + 1);
        let count = cache.entries.len();
        cache.render(&large, 100);
        assert_eq!(cache.entries.len(), count);
    }
}
