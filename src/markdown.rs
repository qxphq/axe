//! Markdown.
//!
//! `pulldown-cmark` reads CommonMark, with GFM tables and strikethrough, into
//! events; this module builds them into blocks and inlines, with the limits
//! and the URL check this transcript needs. The renderer turns blocks into
//! transcript lines, wrapped at a given width, 0 meaning unbounded.
//!
//! `Stream` adapts the document parser to a stream of lines. Lines are held
//! until the block they belong to ends, because laying a block out needs all
//! of it: a table its header, a paragraph its last line.

use pulldown_cmark::{Alignment, Event, Options, Parser, Tag};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

// --- Types ---

#[derive(Clone, Copy, PartialEq)]
enum Align {
    Left,
    Center,
    Right,
}

enum Inline {
    Text(String),
    Br,
    Code(String),
    Strong(Vec<Inline>),
    Em(Vec<Inline>),
    Del(Vec<Inline>),
    Link { href: String, children: Vec<Inline> },
    Image { src: String, alt: String },
}

enum Block {
    Heading {
        level: usize,
        content: Vec<Inline>,
    },
    Paragraph(Vec<Inline>),
    Code {
        content: String,
    },
    Blockquote(Vec<Block>),
    List {
        ordered: bool,
        start: usize,
        items: Vec<Vec<Block>>,
    },
    Table {
        headers: Vec<Vec<Inline>>,
        aligns: Vec<Option<Align>>,
        rows: Vec<Vec<Vec<Inline>>>,
    },
    Hr,
}

const MAX_NESTING_DEPTH: usize = 10;
const MAX_TABLE_COLUMNS: usize = 50;
const MAX_TABLE_ROWS: usize = 1000;
const MAX_LIST_ITEMS: usize = 1000;

fn options() -> Options {
    Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH
}

// --- URL safety ---

const SAFE_PROTOCOLS: [&str; 3] = ["https:", "http:", "mailto:"];

fn safe_url(url: &str) -> bool {
    let cleaned: String = url
        .trim()
        .chars()
        .filter(|c| !matches!(c, '\t' | '\n' | '\r'))
        .collect();
    if cleaned.starts_with("//") {
        return false;
    }
    match cleaned.find(':') {
        // A relative URL resolves against the base, so it keeps https:. A
        // scheme only settles the answer when it really is one.
        Some(colon) => {
            let scheme = &cleaned[..colon];
            let real = scheme
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic())
                && scheme
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
            !real
                || SAFE_PROTOCOLS.iter().any(|p| {
                    p.len() == scheme.len() + 1 && p[..scheme.len()].eq_ignore_ascii_case(scheme)
                })
        }
        None => true,
    }
}

// --- Events into blocks ---

/// What an open tag builds. `Skip` is a tag this transcript does not draw,
/// or one nested past `MAX_NESTING_DEPTH`: its content joins its parent.
enum Kind {
    Root,
    Quote,
    Item,
    Paragraph,
    Heading(usize),
    Code,
    List(Option<u64>),
    Table(Vec<Option<Align>>),
    Row,
    Cell,
    Strong,
    Em,
    Del,
    Link(String),
    Image(String),
    Skip,
}

/// An open tag and what it has collected so far.
struct Node {
    kind: Kind,
    blocks: Vec<Block>,
    inlines: Vec<Inline>,
    items: Vec<Vec<Block>>,
    cells: Vec<Vec<Inline>>,
    rows: Vec<Vec<Vec<Inline>>>,
    code: String,
}

impl Node {
    fn new(kind: Kind) -> Node {
        Node {
            kind,
            blocks: Vec::new(),
            inlines: Vec::new(),
            items: Vec::new(),
            cells: Vec::new(),
            rows: Vec::new(),
            code: String::new(),
        }
    }

    /// Text a container holds outside any paragraph — a tight item's, an
    /// HTML block's — is a paragraph of its own.
    fn settle(&mut self) {
        let inlines = std::mem::take(&mut self.inlines);
        if visible(&inlines) {
            self.blocks.push(Block::Paragraph(inlines));
        }
    }

    fn block(&mut self, block: Block) {
        self.settle();
        self.blocks.push(block);
    }
}

fn visible(inlines: &[Inline]) -> bool {
    inlines
        .iter()
        .any(|i| !matches!(i, Inline::Text(t) if t.trim().is_empty()))
}

fn align(a: &Alignment) -> Option<Align> {
    match a {
        Alignment::None => None,
        Alignment::Left => Some(Align::Left),
        Alignment::Center => Some(Align::Center),
        Alignment::Right => Some(Align::Right),
    }
}

/// The node a tag opens under `parent`, at `depth` open tags. A paragraph,
/// heading or code block holds only text, and an item, row or cell is as
/// deep as the list or table around it, so only the rest is capped.
fn kind_of(tag: Tag, parent: &Kind, depth: usize) -> Kind {
    let capped = depth > MAX_NESTING_DEPTH;
    match tag {
        Tag::Paragraph | Tag::HtmlBlock => Kind::Paragraph,
        Tag::Heading { level, .. } => Kind::Heading(level as usize),
        Tag::CodeBlock(_) => Kind::Code,
        Tag::Item if !matches!(parent, Kind::Skip) => Kind::Item,
        Tag::TableHead | Tag::TableRow if !matches!(parent, Kind::Skip) => Kind::Row,
        Tag::TableCell if !matches!(parent, Kind::Skip) => Kind::Cell,
        _ if capped => Kind::Skip,
        Tag::BlockQuote(_) => Kind::Quote,
        Tag::List(start) => Kind::List(start),
        Tag::Table(aligns) => {
            Kind::Table(aligns.iter().take(MAX_TABLE_COLUMNS).map(align).collect())
        }
        Tag::Emphasis => Kind::Em,
        Tag::Strong => Kind::Strong,
        Tag::Strikethrough => Kind::Del,
        Tag::Link { dest_url, .. } => Kind::Link(dest_url.into_string()),
        Tag::Image { dest_url, .. } => Kind::Image(dest_url.into_string()),
        _ => Kind::Skip,
    }
}

/// A closed node, handed to the node it sits in.
fn close(mut node: Node, parent: &mut Node) {
    match node.kind {
        Kind::Root => {}
        Kind::Paragraph => {
            if visible(&node.inlines) {
                parent.block(Block::Paragraph(node.inlines));
            }
        }
        Kind::Heading(level) => parent.block(Block::Heading {
            level,
            content: node.inlines,
        }),
        Kind::Code => {
            let mut content = node.code;
            if content.ends_with('\n') {
                content.pop();
            }
            parent.block(Block::Code { content });
        }
        Kind::Quote => {
            node.settle();
            parent.block(Block::Blockquote(node.blocks));
        }
        Kind::Item => {
            node.settle();
            if parent.items.len() < MAX_LIST_ITEMS {
                parent.items.push(node.blocks);
            }
        }
        Kind::List(start) => parent.block(Block::List {
            ordered: start.is_some(),
            start: start.map_or(1, |n| n as usize),
            items: node.items,
        }),
        Kind::Table(aligns) => {
            let mut rows = node.rows.into_iter();
            parent.block(Block::Table {
                headers: rows.next().unwrap_or_default(),
                aligns,
                rows: rows.collect(),
            });
        }
        Kind::Row => {
            if parent.rows.len() <= MAX_TABLE_ROWS {
                parent.rows.push(node.cells);
            }
        }
        Kind::Cell => {
            if parent.cells.len() < MAX_TABLE_COLUMNS {
                parent.cells.push(node.inlines);
            }
        }
        Kind::Strong => parent.inlines.push(Inline::Strong(node.inlines)),
        Kind::Em => parent.inlines.push(Inline::Em(node.inlines)),
        Kind::Del => parent.inlines.push(Inline::Del(node.inlines)),
        Kind::Link(href) if safe_url(&href) => parent.inlines.push(Inline::Link {
            href,
            children: node.inlines,
        }),
        Kind::Image(src) if safe_url(&src) => parent.inlines.push(Inline::Image {
            alt: plain_inlines(&node.inlines),
            src,
        }),
        // An unsafe target is shown, never linked.
        Kind::Link(url) | Kind::Image(url) => {
            parent.inlines.append(&mut node.inlines);
            parent.inlines.push(Inline::Text(format!(" ({url})")));
        }
        Kind::Skip => {
            if node.blocks.is_empty() {
                parent.inlines.append(&mut node.inlines);
            } else {
                node.settle();
                for block in node.blocks {
                    parent.block(block);
                }
            }
        }
    }
}

fn plain_inlines(inlines: &[Inline]) -> String {
    let mut out = String::new();
    for item in inlines {
        match item {
            Inline::Text(t) | Inline::Code(t) => out.push_str(t),
            Inline::Br => out.push(' '),
            Inline::Strong(c) | Inline::Em(c) | Inline::Del(c) => out.push_str(&plain_inlines(c)),
            Inline::Link { children, .. } => out.push_str(&plain_inlines(children)),
            Inline::Image { alt, .. } => out.push_str(alt),
        }
    }
    out
}

/// HTML is not drawn: `<br>` is a line break, any other tag is dropped and
/// the text between tags stays.
fn html(raw: &str) -> Option<Inline> {
    let lower = raw.trim().to_ascii_lowercase();
    let br = lower
        .strip_prefix("<br")
        .and_then(|rest| rest.strip_suffix('>'))
        .is_some_and(|rest| rest.trim().trim_end_matches('/').trim().is_empty());
    if br {
        return Some(Inline::Br);
    }
    let mut out = String::new();
    let mut tag = false;
    for c in raw.chars() {
        match c {
            '<' => tag = true,
            '>' if tag => tag = false,
            _ if !tag => out.push(c),
            _ => {}
        }
    }
    (!out.is_empty()).then_some(Inline::Text(out))
}

fn parse(text: &str) -> Vec<Block> {
    let mut stack = vec![Node::new(Kind::Root)];
    for event in Parser::new_ext(text, options()) {
        let top = stack.len() - 1;
        match event {
            Event::Start(tag) => {
                let kind = kind_of(tag, &stack[top].kind, top);
                stack.push(Node::new(kind));
            }
            Event::End(_) => {
                if top > 0 {
                    let node = stack.pop().unwrap_or_else(|| Node::new(Kind::Skip));
                    close(node, &mut stack[top - 1]);
                }
            }
            Event::Text(t) if matches!(stack[top].kind, Kind::Code) => stack[top].code.push_str(&t),
            Event::Text(t) => stack[top].inlines.push(Inline::Text(t.into_string())),
            Event::Code(t) => stack[top].inlines.push(Inline::Code(t.into_string())),
            Event::Html(t) | Event::InlineHtml(t) => {
                if let Some(inline) = html(&t) {
                    stack[top].inlines.push(inline);
                }
            }
            Event::SoftBreak => stack[top].inlines.push(Inline::Text("\n".to_string())),
            Event::HardBreak => stack[top].inlines.push(Inline::Br),
            Event::Rule => stack[top].block(Block::Hr),
            _ => {}
        }
    }
    // The parser closes every tag it opens, so only the root is left.
    let mut root = stack.swap_remove(0);
    root.settle();
    root.blocks
}

/// How many top-level blocks `text` holds, and whether the last is a list.
fn top_level(text: &str) -> (usize, bool) {
    let mut depth = 0usize;
    let mut count = 0;
    let mut list = false;
    for event in Parser::new_ext(text, options()) {
        match event {
            Event::Start(tag) => {
                if depth == 0 {
                    count += 1;
                    list = matches!(tag, Tag::List(_));
                }
                depth += 1;
            }
            Event::End(_) => depth = depth.saturating_sub(1),
            Event::Rule if depth == 0 => {
                count += 1;
                list = false;
            }
            _ => {}
        }
    }
    (count, list)
}

// --- Styles ---

const BOLD: Style = Style::new().add_modifier(Modifier::BOLD);
const ITALIC: Style = Style::new().add_modifier(Modifier::ITALIC);
const STRIKE: Style = Style::new().add_modifier(Modifier::CROSSED_OUT);
const DIM: Style = Style::new().add_modifier(Modifier::DIM);
const CODE: Style = Style::new().fg(Color::Green);
const LINK: Style = Style::new()
    .fg(Color::Cyan)
    .add_modifier(Modifier::UNDERLINED);

// --- Renderer ---

#[derive(Clone)]
enum Piece {
    Text(Style, String),
    Break,
}

/// One message, laid out at `width`. Top-level blocks are separated by one
/// blank line: markdown's own spacing, not the caller's.
pub fn document(text: &str, width: usize) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    render(&parse(text), width, &mut out);
    out
}

fn render(blocks: &[Block], width: usize, out: &mut Vec<Line<'static>>) {
    for (k, block) in blocks.iter().enumerate() {
        if k > 0 {
            out.push(Line::default());
        }
        emit(std::slice::from_ref(block), width, Style::new(), &[], out);
    }
}

fn emit(
    blocks: &[Block],
    width: usize,
    base: Style,
    gutter: &[Span<'static>],
    out: &mut Vec<Line<'static>>,
) {
    let pad = spans_width(gutter);
    let inner = if width == 0 {
        0
    } else {
        width.saturating_sub(pad).max(1)
    };
    for block in blocks {
        match block {
            Block::Paragraph(content) => {
                push_rows(out, gutter, wrap(&pieces(content, base), inner));
            }
            Block::Heading { level, content } => {
                let style = if *level <= 2 {
                    base.patch(BOLD)
                        .patch(Style::new().add_modifier(Modifier::UNDERLINED))
                } else {
                    base.patch(BOLD)
                };
                push_rows(out, gutter, wrap(&pieces(content, style), inner));
            }
            Block::Hr => {
                let n = if inner == 0 { 40 } else { inner };
                let mut spans = gutter.to_vec();
                spans.push(Span::styled("─".repeat(n), base.patch(DIM)));
                out.push(Line::from(spans));
            }
            Block::Code { content } => {
                // A row too wide for the field takes the rows it needs, gutter
                // and all, the way a paragraph does: left to the screen, the
                // continuation would start at the terminal's edge and leave the
                // quote or the item it sits in.
                for line in content.split('\n') {
                    for row in wrap_code(line, inner) {
                        let mut spans = gutter.to_vec();
                        spans.push(Span::styled(row, base.patch(CODE)));
                        out.push(Line::from(spans));
                    }
                }
            }
            Block::Blockquote(inner_blocks) => {
                let style = base.patch(DIM);
                let mut g = gutter.to_vec();
                g.push(Span::styled("│ ", style));
                emit(inner_blocks, width, style, &g, out);
            }
            Block::List {
                ordered,
                start,
                items,
            } => {
                for (k, item) in items.iter().enumerate() {
                    let marker = if *ordered {
                        format!("{}. ", start + k)
                    } else {
                        "• ".to_string()
                    };
                    let marker_width = text_width(&marker);
                    let mut first = gutter.to_vec();
                    first.push(Span::styled(marker, base.patch(DIM)));
                    let mut rest = gutter.to_vec();
                    rest.push(Span::raw(" ".repeat(marker_width)));
                    let sub = if width == 0 {
                        0
                    } else {
                        inner.saturating_sub(marker_width).max(1)
                    };
                    let mut laid = Vec::new();
                    emit(item, sub, base, &[], &mut laid);
                    for (n, line) in laid.into_iter().enumerate() {
                        let head = if n == 0 { &first } else { &rest };
                        let mut spans = head.clone();
                        spans.extend(line.spans);
                        out.push(Line::from(spans));
                    }
                }
            }
            Block::Table {
                headers,
                aligns,
                rows,
            } => {
                table(headers, aligns, rows, base, inner, gutter, out);
            }
        }
    }
}

fn push_rows(out: &mut Vec<Line<'static>>, gutter: &[Span<'static>], rows: Vec<Line<'static>>) {
    for row in rows {
        let mut spans = gutter.to_vec();
        spans.extend(row.spans);
        out.push(Line::from(spans));
    }
}

fn pieces(inlines: &[Inline], base: Style) -> Vec<Piece> {
    let mut out = Vec::new();
    flatten(inlines, base, &mut out);
    out
}

fn flatten(inlines: &[Inline], base: Style, out: &mut Vec<Piece>) {
    for item in inlines {
        match item {
            Inline::Text(text) => out.push(Piece::Text(base, text.clone())),
            Inline::Br => out.push(Piece::Break),
            Inline::Code(code) => out.push(Piece::Text(base.patch(CODE), code.clone())),
            Inline::Strong(children) => flatten(children, base.patch(BOLD), out),
            Inline::Em(children) => flatten(children, base.patch(ITALIC), out),
            Inline::Del(children) => flatten(children, base.patch(STRIKE), out),
            Inline::Link { href, children } => {
                let start = out.len();
                flatten(children, base.patch(LINK), out);
                if plain_pieces(&out[start..]) != *href {
                    out.push(Piece::Text(base.patch(DIM), format!(" ({href})")));
                }
            }
            Inline::Image { src, alt } => {
                out.push(Piece::Text(base.patch(LINK), alt.clone()));
                if alt != src {
                    out.push(Piece::Text(base.patch(DIM), format!(" ({src})")));
                }
            }
        }
    }
}

fn plain_pieces(items: &[Piece]) -> String {
    let mut out = String::new();
    for item in items {
        match item {
            Piece::Text(_, text) => out.push_str(text),
            Piece::Break => out.push(' '),
        }
    }
    out
}

fn piece_spans(items: &[Piece]) -> Vec<Span<'static>> {
    items
        .iter()
        .map(|item| match item {
            Piece::Text(style, text) => Span::styled(text.clone(), *style),
            Piece::Break => Span::raw(" ".to_string()),
        })
        .collect()
}

fn plain_width(items: &[Piece]) -> usize {
    items
        .iter()
        .map(|item| match item {
            Piece::Text(_, text) => text_width(text),
            Piece::Break => 1,
        })
        .sum()
}

fn cell_width(c: char) -> usize {
    if c == '\n' {
        1
    } else {
        c.width().unwrap_or(1)
    }
}

fn text_width(text: &str) -> usize {
    text.chars().map(cell_width).sum()
}

fn spans_width(spans: &[Span]) -> usize {
    spans.iter().map(|s| text_width(&s.content)).sum()
}

/// Greedy wrap: words move whole, a word wider than the row is split. Runs of
/// whitespace collapse to one space, and a soft line break is whitespace, so a
/// hard-wrapped paragraph reflows instead of staying ragged.
struct Wrap {
    width: usize,
    rows: Vec<Line<'static>>,
    row: Vec<Span<'static>>,
    run: String,
    style: Style,
    col: usize,
    space: bool,
}

impl Wrap {
    fn new(width: usize) -> Wrap {
        Wrap {
            width,
            rows: Vec::new(),
            row: Vec::new(),
            run: String::new(),
            style: Style::new(),
            col: 0,
            space: false,
        }
    }

    fn flush_run(&mut self) {
        if !self.run.is_empty() {
            let text = std::mem::take(&mut self.run);
            self.row.push(Span::styled(text, self.style));
        }
    }

    fn break_row(&mut self) {
        self.flush_run();
        self.rows.push(Line::from(std::mem::take(&mut self.row)));
        self.col = 0;
        self.space = false;
    }

    fn put(&mut self, style: Style, c: char) {
        if !self.run.is_empty() && self.style != style {
            self.flush_run();
        }
        self.style = style;
        self.col += cell_width(c);
        self.run.push(c);
    }

    fn text(&mut self, style: Style, text: &str) {
        let mut word = String::new();
        for c in text.chars() {
            if c.is_whitespace() {
                self.word(style, &mut word);
                self.space = true;
                continue;
            }
            word.push(c);
        }
        self.word(style, &mut word);
    }

    /// A word moves whole; only one wider than the row is split, and that is
    /// the one case where a break falls inside it.
    fn word(&mut self, style: Style, word: &mut String) {
        if word.is_empty() {
            return;
        }
        let w = text_width(word);
        if self.width > 0 && self.col > 0 {
            let lead = usize::from(self.space);
            if self.col + lead + w > self.width {
                self.break_row();
            }
        }
        if self.space && self.col > 0 {
            self.put(style, ' ');
        }
        self.space = false;
        for c in word.chars() {
            if self.width > 0 && self.col >= self.width {
                self.break_row();
            }
            self.put(style, c);
        }
        word.clear();
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        if !self.row.is_empty() || !self.run.is_empty() {
            self.break_row();
        }
        self.rows
    }
}

fn wrap(items: &[Piece], width: usize) -> Vec<Line<'static>> {
    let mut writer = Wrap::new(width);
    for item in items {
        match item {
            Piece::Text(style, text) => writer.text(*style, text),
            Piece::Break => writer.break_row(),
        }
    }
    writer.finish()
}

/// Headers, a separator, then rows. Column widths come from the content, then
/// the widest columns give way until the table fits.
fn table(
    headers: &[Vec<Inline>],
    aligns: &[Option<Align>],
    rows: &[Vec<Vec<Inline>>],
    base: Style,
    width: usize,
    gutter: &[Span<'static>],
    out: &mut Vec<Line<'static>>,
) {
    let cols = headers
        .len()
        .max(rows.iter().map(Vec::len).max().unwrap_or(0));
    if cols == 0 {
        return;
    }
    let mut grid: Vec<Vec<Vec<Piece>>> = Vec::new();
    grid.push(
        headers
            .iter()
            .map(|c| pieces(c, base.patch(BOLD)))
            .collect(),
    );
    for row in rows {
        grid.push(row.iter().map(|c| pieces(c, base)).collect());
    }
    let mut widths = vec![1usize; cols];
    for row in &grid {
        for (k, cell) in row.iter().enumerate() {
            widths[k] = widths[k].max(plain_width(cell));
        }
    }
    if width > 0 {
        let mut total: usize = widths.iter().map(|w| w + 2).sum::<usize>() + cols - 1;
        while total > width {
            let Some((k, _)) = widths.iter().enumerate().max_by_key(|(_, w)| **w) else {
                break;
            };
            if widths[k] <= 1 {
                break;
            }
            widths[k] -= 1;
            total -= 1;
        }
    }
    let border = base.patch(DIM);
    let empty: &[Piece] = &[];
    for (r, row) in grid.iter().enumerate() {
        let mut spans = gutter.to_vec();
        for (k, width) in widths.iter().enumerate().take(cols) {
            if k > 0 {
                spans.push(Span::styled("│", border));
            }
            let cell = clip(row.get(k).map(Vec::as_slice).unwrap_or(empty), *width);
            let used = plain_width(&cell);
            let pad = width.saturating_sub(used);
            let (left, right) = match aligns.get(k).copied().flatten() {
                Some(Align::Right) => (pad, 0),
                Some(Align::Center) => (pad / 2, pad - pad / 2),
                _ => (0, pad),
            };
            spans.push(Span::raw(" ".to_string()));
            if left > 0 {
                spans.push(Span::raw(" ".repeat(left)));
            }
            spans.extend(piece_spans(&cell));
            if right > 0 {
                spans.push(Span::raw(" ".repeat(right)));
            }
            spans.push(Span::raw(" ".to_string()));
        }
        out.push(Line::from(spans));
        if r == 0 {
            let mut spans = gutter.to_vec();
            for (k, width) in widths.iter().enumerate().take(cols) {
                if k > 0 {
                    spans.push(Span::styled("┼", border));
                }
                spans.push(Span::styled("─".repeat(*width + 2), border));
            }
            out.push(Line::from(spans));
        }
    }
}

/// A code row broken at `width` cells. Whitespace is content here, so unlike
/// `wrap` nothing collapses and no word moves: the row is cut where the cells
/// run out. 0 is unbounded.
fn wrap_code(text: &str, width: usize) -> Vec<String> {
    let mut rows = Vec::new();
    let mut row = String::new();
    let mut col = 0;
    for c in text.chars() {
        let w = cell_width(c);
        if width > 0 && col > 0 && col + w > width {
            rows.push(std::mem::take(&mut row));
            col = 0;
        }
        row.push(c);
        col += w;
    }
    rows.push(row);
    rows
}

/// A cell wider than its column loses its tail, not its head.
fn clip(items: &[Piece], width: usize) -> Vec<Piece> {
    if plain_width(items) <= width {
        return items.to_vec();
    }
    let limit = width.saturating_sub(1);
    let mut out = Vec::new();
    let mut col = 0;
    'cells: for item in items {
        if let Piece::Text(style, text) = item {
            let mut buf = String::new();
            for c in text.chars() {
                let w = cell_width(c);
                if col + w > limit {
                    if !buf.is_empty() {
                        out.push(Piece::Text(*style, buf));
                    }
                    break 'cells;
                }
                buf.push(c);
                col += w;
            }
            if !buf.is_empty() {
                out.push(Piece::Text(*style, buf));
            }
        }
    }
    out.push(Piece::Text(Style::new(), "…".to_string()));
    out
}

// --- Stream ---

/// The document parser, fed a line at a time. Lines are held until the block
/// they belong to ends: at a blank line the held block cannot continue past,
/// at a line after a gap that the parser puts in a block of its own, or when
/// the caller flushes because something else goes into the transcript. The
/// parser itself answers both, so a stream lays out what the same text at
/// once would.
#[derive(Default)]
pub struct Stream {
    lines: Vec<String>,
    /// Blank lines held since the last one pushed: they may separate two
    /// blocks, or be a list's or an open fence's own gaps.
    blank: usize,
}

impl Stream {
    pub fn new() -> Stream {
        Stream::default()
    }

    pub fn push(&mut self, line: &str, width: usize, out: &mut Vec<Line<'static>>) {
        if line.trim().is_empty() {
            self.blank += 1;
            if !self.waits() {
                self.flush(width, out);
            }
            return;
        }
        self.separate(line, width, out);
        self.lines.push(line.to_string());
    }

    /// Whether the held block may still take a line after a gap: a list, whose
    /// next item may follow, or anything a line after a gap would join, such
    /// as a fence not closed yet.
    fn waits(&self) -> bool {
        if self.lines.is_empty() {
            return false;
        }
        let held = self.lines.join("\n");
        let (count, list) = top_level(&held);
        list || top_level(&format!("{held}\n\nx")).0 == count
    }

    /// Settle the blank lines held before `line`: they join the buffer when
    /// the line still belongs to the block held, and otherwise the block ends
    /// here, before the new line.
    fn separate(&mut self, line: &str, width: usize, out: &mut Vec<Line<'static>>) {
        if self.blank == 0 {
            return;
        }
        let held = self.lines.join("\n");
        let gap = "\n".repeat(self.blank + 1);
        if top_level(&format!("{held}{gap}{line}")).0 == top_level(&held).0 {
            for _ in 0..std::mem::take(&mut self.blank) {
                self.lines.push(String::new());
            }
        } else {
            self.flush(width, out);
        }
    }

    pub fn flush(&mut self, width: usize, out: &mut Vec<Line<'static>>) {
        self.blank = 0;
        if self.lines.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.lines).join("\n");
        render(&parse(&text), width, out);
    }
}

// --- Output for a stream that has no screen ---

#[cfg(test)]
fn plain(line: &Line) -> String {
    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    text.trim_end().to_string()
}

/// A rendered line as ANSI, for the paths that have no screen. The codes are
/// `text::ansi`'s: this module decides what is code and what is a link, the
/// terminal's own names decide how that looks.
pub fn ansi(line: &Line) -> String {
    let mut out = String::new();
    for span in &line.spans {
        out.push_str(&crate::text::ansi(&span.content, span.style));
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(input: &str) -> String {
        document(input, 80)
            .iter()
            .map(plain)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn single_tilde_does_not_hide_double_strike() {
        assert_eq!(text("~a ~~b~~"), "~a b");
    }

    #[test]
    fn double_strike_does_not_hide_single_tilde() {
        assert_eq!(text("~~a~~ ~b~"), "a b");
    }

    #[test]
    fn blocks_render() {
        let out = text("# Heading\n\n- one\n- two\n\n| a | b |\n|---|---|\n| 1 | 2 |");
        assert!(out.contains("Heading"), "{out}");
        assert!(out.contains("• one"), "{out}");
        assert!(out.contains("a │ b"), "{out}");
    }

    #[test]
    fn code_and_links_render() {
        let out = text("`a & b` [x](https://example.com) [bad](javascript:alert(1))");
        assert!(out.contains("a & b"), "{out}");
        assert!(out.contains("(https://example.com)"), "{out}");
        assert!(out.contains("javascript:alert(1)"), "{out}");
        assert!(text("```\nlet x = 1;\n```").contains("let x = 1;"));
    }

    #[test]
    fn stream_lays_out_when_block_ends() {
        let mut stream = Stream::new();
        let mut out = Vec::new();
        stream.push("hello", 80, &mut out);
        assert!(out.is_empty());
        stream.push("world", 80, &mut out);
        stream.push("", 80, &mut out);
        let rendered: Vec<String> = out.iter().map(plain).collect();
        assert_eq!(rendered, vec!["hello world"]);
    }

    #[test]
    fn bare_urls_entities_and_breaks() {
        let out = text("see https://example.com/x &amp; more<br>next");
        assert!(out.contains("https://example.com/x"), "{out}");
        assert!(out.contains("&"), "{out}");
        assert!(out.lines().count() >= 2, "{out}");
    }

    #[test]
    fn details_and_heading_closers_render() {
        let out = text("<details><summary>More</summary>\nBody\n</details>");
        assert!(out.contains("More"), "{out}");
        assert!(out.contains("Body"), "{out}");
        assert_eq!(text("## Title ##"), "Title");
        assert!(text("---").contains("─"));
    }

    #[test]
    fn only_a_real_and_safe_scheme_links() {
        for url in [
            "https://e/x",
            "http://e",
            "mailto:a@b",
            "/relative",
            "#frag",
            "e/x",
        ] {
            assert!(safe_url(url), "{url}");
        }
        for url in [
            "javascript:alert(1)",
            "data:text/html,x",
            "//host/x",
            "file:/etc",
        ] {
            assert!(!safe_url(url), "{url}");
        }
        // Case does not decide it, and neither does leading whitespace.
        assert!(safe_url("  HTTPS://e/x "));
        // A tab or a newline inside a scheme is stripped before the check.
        assert!(safe_url("ht\ttps://e/x"));
        // A colon after one letter is still a scheme, and it is not on the list.
        assert!(!safe_url("a:b/c"));
        // A scheme starts with a letter: `1http:` is a path with a colon in it.
        assert!(safe_url("1http:x"));
        assert!(!safe_url("c++:x"), "a real scheme outside the list");
    }

    #[test]
    fn inline_marks_nest_and_unmatched_ones_stand() {
        let out = text("**bold** and *italic* and _under_ and __strong__ and ~~gone~~");
        assert_eq!(out, "bold and italic and under and strong and gone");
        // An asterisk with a space on each side is text, not emphasis.
        assert_eq!(text("a * b"), "a * b");
        assert_eq!(text("2 * 3 * 4"), "2 * 3 * 4");
        // Inside a word an underscore is text.
        assert_eq!(text("a_b_c"), "a_b_c");
        // Entities decode.
        assert_eq!(text("a &amp; b &#x41; &nope;"), "a & b A &nope;");
        // Emphasis inside a link stays inside it.
        let out = text("[**bold link**](https://e/x)");
        assert!(out.contains("bold link"), "{out}");
        assert!(out.contains("(https://e/x)"), "{out}");
    }

    #[test]
    fn a_link_shows_its_target_and_keeps_an_unsafe_one_as_text() {
        let out = text("[docs](https://example.com/a)");
        assert!(out.contains("docs"), "{out}");
        assert!(out.contains("(https://example.com/a)"), "{out}");
        let out = text("[click](javascript:alert(1))");
        assert!(out.contains("click"), "{out}");
        assert!(out.contains("(javascript:alert(1))"), "{out}");
        // The difference is the style, not the text: the unsafe one keeps the
        // base style, so no span of the line is a link.
        let line = &document("[click](javascript:alert(1))", 80)[0];
        assert!(
            line.spans
                .iter()
                .all(|span| span.style.fg != Some(Color::Cyan)),
            "{:?}",
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        );
        // An image is a link with a description.
        let out = text("![alt](https://e/i.png)");
        assert!(out.contains("alt"), "{out}");
    }

    #[test]
    fn a_code_span_takes_its_own_backticks() {
        assert_eq!(text("``a ` b``"), "a ` b");
        assert_eq!(text("`one`"), "one");
        // An unclosed backtick is text.
        assert_eq!(text("a ` b"), "a ` b");
        // Backslash escapes the mark that follows it.
        assert_eq!(text("\\*not italic\\*"), "*not italic*");
    }

    #[test]
    fn a_fence_keeps_what_is_inside_it() {
        let out = text("```rust\nlet x = 1;\n  indented\n```");
        assert_eq!(out, "let x = 1;\n  indented");
        // An unclosed fence runs to the end of the message.
        let out = text("~~~\nabc\n\ndef");
        assert!(out.contains("abc"), "{out}");
        assert!(out.contains("def"), "{out}");
        // A fence of one marker is closed only by one as long.
        let out = text("````\n```\ninner\n````");
        assert!(out.contains("```"), "{out}");
        assert!(out.contains("inner"), "{out}");
    }

    #[test]
    fn headings_lists_and_rules_take_their_forms() {
        let out = text("# One\n\n### Three ###\n\n---");
        assert!(out.contains("One"), "{out}");
        assert!(out.contains("Three"), "{out}");
        assert!(!out.contains('#'), "{out}");
        assert!(out.lines().any(|line| line.starts_with('─')), "{out}");
        for marks in ["***", "___", "- - -"] {
            assert!(text(marks).starts_with('─'), "{marks} is a rule");
        }
        assert_eq!(text("###Four"), "###Four", "a heading needs a space");
        assert_eq!(text("####### seven"), "####### seven");

        let out = text("- one\n- two\n  - nested");
        assert!(out.contains("• one"), "{out}");
        assert!(out.contains("  • nested"), "{out}");

        let out = text("1. first\n2) second");
        assert!(out.contains("1. first"), "{out}");
        assert!(out.contains("2. second"), "{out}");
        assert!(!out.contains("3. third"));
        // An ordered list keeps its own start number.
        let out = text("3. three\n4. four");
        assert!(out.contains("3. three"), "{out}");
        assert!(out.contains("4. four"), "{out}");

        let out = text("> quoted\n>\n> more");
        assert!(out.contains("│ quoted"), "{out}");
        assert!(out.contains("│ more"), "{out}");
    }

    #[test]
    fn a_table_is_drawn_with_its_columns_and_alignment() {
        let out = text("| a | b |\n|:--|--:|\n| 1 | 2 |\n| 3 |");
        assert!(out.contains("a │ b"), "{out}");
        assert!(out.contains("─┼─"), "{out}");
        assert!(out.contains("1 │ 2"), "{out}");
        // A short row is padded, not merged into the next column.
        assert!(out.contains("3 │"), "{out}");
        // A cell escapes the pipe that would split it.
        let out = text("| a \\| b | c |\n|---|---|\n| x | y |");
        assert!(out.contains("a | b"), "{out}");
        // A table with no body is still a table.
        let out = text("| h |\n|---|");
        assert!(out.trim().lines().count() == 2, "{out}");
        // Alignment pads on the side the delimiter row names.
        let out: Vec<String> = document("| aaa | b | c |\n|:-:|--:|:--|\n| x | y | z |", 80)
            .iter()
            .map(plain)
            .collect();
        assert_eq!(out[2], "  x  │ y │ z");
    }

    #[test]
    fn details_keep_the_summary_and_their_blocks() {
        let out = text("<details><summary>The summary</summary>\n\n- inside\n</details>");
        assert!(out.contains("The summary"), "{out}");
        assert!(out.contains("• inside"), "{out}");
        assert!(!out.contains('<'), "{out}");
    }

    #[test]
    fn a_long_line_is_clipped_and_a_long_word_broken() {
        let long = "one two three four five six seven eight nine ten";
        let out = document(long, 12);
        let rendered: Vec<String> = out.iter().map(plain).collect();
        assert!(rendered.len() > 1, "{rendered:?}");
        for line in &rendered {
            assert!(text_width(line) <= 12, "{line:?}");
        }
        // A word wider than the line is broken, not dropped.
        let out: Vec<String> = document("supercalifragilistic", 6)
            .iter()
            .map(plain)
            .collect();
        assert_eq!(out.join(""), "supercalifragilistic");

        // A table cell that does not fit loses its tail and says so, and a
        // table is narrowed until it fits the width it is given.
        let table = "| header | b |\n|---|---|\n| a very long cell indeed | 2 |";
        let out: String = document(table, 16)
            .iter()
            .map(plain)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(out.contains('…'), "{out}");
        assert!(out.contains("header"), "{out}");
        for line in document(table, 16).iter().map(plain) {
            assert!(text_width(&line) <= 16, "{line:?}");
        }
    }

    #[test]
    fn ansi_output_is_plain_when_the_output_is_not_a_terminal() {
        // A redirect gets no escapes: the text is the same, only the codes are
        // gone. The codes are asserted on `styled`, which does not read stdout,
        // so the test does not depend on whatever the runner gave it.
        let line = &document("**bold** `code`", 80)[0];
        assert_eq!(plain(line), "bold code");
        let styled: String = line
            .spans
            .iter()
            .map(|span| crate::text::styled(&span.content, span.style))
            .collect();
        assert_eq!(styled.trim_end(), "\x1b[1mbold\x1b[0m\x1b[32m code\x1b[0m");
    }

    #[test]
    fn a_stream_lays_out_each_block_when_it_ends() {
        let mut stream = Stream::new();
        let mut out = Vec::new();
        stream.push("```", 20, &mut out);
        stream.push("code", 20, &mut out);
        stream.push("```", 20, &mut out);
        assert!(out.is_empty(), "the fence is not a block yet");
        stream.push("", 20, &mut out);
        assert_eq!(out.iter().map(plain).collect::<Vec<_>>(), vec!["code"]);
        // Flushing ends the block that is still open.
        let mut stream = Stream::new();
        let mut out = Vec::new();
        stream.push("tail without a blank line", 20, &mut out);
        assert!(out.is_empty());
        stream.flush(20, &mut out);
        let rendered: String = out.iter().map(plain).collect::<Vec<_>>().join(" ");
        assert_eq!(rendered, "tail without a blank line");
        // Nesting past the limit keeps the text, not the structure.
        assert!(text(&format!("{} deep", ">".repeat(1000))).ends_with("deep"));
        assert!(text(&format!("{}deep{}", "*".repeat(500), "*".repeat(500))).contains("deep"));
        assert!(parse("").is_empty());
    }

    /// A blank line inside a list is the list's own gap, not a block boundary:
    /// the stream must lay out what the same text at once would be.
    #[test]
    fn a_stream_keeps_a_loose_list_whole() {
        let at_once: Vec<String> = document("- a\n\n- b", 20).iter().map(plain).collect();
        assert_eq!(at_once, vec!["• a", "• b"], "one list, no gap in it");

        let mut stream = Stream::new();
        let mut out = Vec::new();
        for line in ["- a", "", "- b"] {
            stream.push(line, 20, &mut out);
        }
        stream.flush(20, &mut out);
        assert_eq!(out.iter().map(plain).collect::<Vec<_>>(), at_once);

        // A gap between two paragraphs is still a boundary, and the list after
        // one is a block of its own. The blank row between two blocks is the
        // writer's, so the rows here sit next to each other.
        let mut stream = Stream::new();
        let mut out = Vec::new();
        for line in ["text", "", "- a"] {
            stream.push(line, 20, &mut out);
        }
        stream.flush(20, &mut out);
        assert_eq!(
            out.iter().map(plain).collect::<Vec<_>>(),
            vec!["text", "• a"]
        );

        // An item indented under another after a gap is nested in it, at once
        // and streamed alike.
        let at_once: Vec<String> = document("- a\n\n  - b", 20).iter().map(plain).collect();
        assert_eq!(at_once, vec!["• a", "  • b"]);
        let mut stream = Stream::new();
        let mut out = Vec::new();
        for line in ["- a", "", "  - b"] {
            stream.push(line, 20, &mut out);
        }
        stream.flush(20, &mut out);
        assert_eq!(out.iter().map(plain).collect::<Vec<_>>(), at_once);

        // A gap inside an open fence is the code's own.
        let mut stream = Stream::new();
        let mut out = Vec::new();
        for line in ["```", "a", "", "b"] {
            stream.push(line, 20, &mut out);
        }
        assert!(out.is_empty());
        stream.flush(20, &mut out);
        assert_eq!(
            out.iter().map(plain).collect::<Vec<_>>(),
            vec!["a", "", "b"]
        );
    }

    /// Code keeps its own whitespace and its block when a row is too wide: the
    /// rows it takes carry the gutter, so a quoted block stays quoted.
    #[test]
    fn a_code_row_wraps_inside_its_block() {
        let quoted = "> ```\n> aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n> ```";
        let rows: Vec<String> = document(quoted, 12).iter().map(plain).collect();
        assert!(rows.len() > 1, "{rows:?}");
        for row in &rows {
            assert!(text_width(row) <= 12, "{row:?}");
            assert!(row.starts_with("│ "), "{row:?}");
        }
        // Spaces are content in code: nothing collapses, nothing moves.
        let rows: Vec<String> = document("```\n  a    b\n```", 40)
            .iter()
            .map(plain)
            .collect();
        assert_eq!(rows, vec!["  a    b"]);
    }
}
