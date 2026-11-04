//! Markdown.
//!
//! The parser is a port of `md.ts`: same block and inline rules, same limits,
//! same URL check, same entity table, so the two cannot drift apart in
//! behavior. The renderer is ours: blocks to transcript lines, wrapped at a
//! given width, 0 meaning unbounded.
//!
//! `Stream` adapts the document parser to a stream of lines. Lines are held
//! until the block they belong to ends, because laying a block out needs all
//! of it: a table its header, a paragraph its last line.

use std::collections::HashMap;
use std::io::{self, IsTerminal};

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
        lang: String,
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
    Details {
        summary: Vec<Inline>,
        blocks: Vec<Block>,
    },
}

const MAX_NESTING_DEPTH: usize = 10;
const MAX_TABLE_COLUMNS: usize = 50;
const MAX_TABLE_ROWS: usize = 1000;
const MAX_LIST_ITEMS: usize = 1000;

// --- Char helpers ---

fn as_chars(s: &str) -> Vec<char> {
    s.chars().collect()
}

fn text_of(c: &[char]) -> String {
    c.iter().collect()
}

fn at(c: &[char], i: usize) -> char {
    c.get(i).copied().unwrap_or('\0')
}

fn starts(c: &[char], i: usize, pat: &str) -> bool {
    pat.chars()
        .enumerate()
        .all(|(k, p)| c.get(i + k) == Some(&p))
}

fn find_from(c: &[char], from: usize, pat: &str) -> Option<usize> {
    let n = pat.chars().count();
    if n == 0 || from > c.len() {
        return None;
    }
    (from..=c.len().saturating_sub(n)).find(|&i| starts(c, i, pat))
}

fn backticks(n: usize) -> String {
    "`".repeat(n)
}

// --- Entity decoding ---

fn named_entity(name: &str) -> Option<&'static str> {
    Some(match name {
        "amp" => "&",
        "lt" => "<",
        "gt" => ">",
        "quot" => "\"",
        "apos" => "'",
        "nbsp" => "\u{a0}",
        "copy" => "\u{a9}",
        "mdash" => "\u{2014}",
        "ndash" => "\u{2013}",
        "hellip" => "\u{2026}",
        "middot" => "\u{b7}",
        "bull" => "\u{2022}",
        "laquo" => "\u{ab}",
        "raquo" => "\u{bb}",
        _ => return None,
    })
}

/// Reject surrogates and out-of-range values, as the JS version does.
fn codepoint(cp: u32) -> Option<char> {
    if cp == 0 || cp > 0x10ffff || (0xd800..=0xdfff).contains(&cp) {
        return None;
    }
    char::from_u32(cp)
}

pub(crate) fn decode_entities(text: &str) -> String {
    let c = as_chars(text);
    let mut out = String::new();
    let mut i = 0;
    while i < c.len() {
        if c[i] == '&' {
            if let Some((ch, next)) = entity_at(&c, i) {
                out.push(ch);
                i = next;
                continue;
            }
        }
        out.push(c[i]);
        i += 1;
    }
    out
}

/// `&(?:#(\d+)|#x([0-9a-fA-F]+)|(\w+));` at `i`, decoded.
fn entity_at(c: &[char], i: usize) -> Option<(char, usize)> {
    let mut j = i + 1;
    if at(c, j) == '#' {
        let hex = matches!(at(c, j + 1), 'x' | 'X');
        j += if hex { 2 } else { 1 };
        let start = j;
        while j < c.len()
            && if hex {
                c[j].is_ascii_hexdigit()
            } else {
                c[j].is_ascii_digit()
            }
        {
            j += 1;
        }
        if j == start || at(c, j) != ';' {
            return None;
        }
        let digits = text_of(&c[start..j]);
        let cp = u32::from_str_radix(&digits, if hex { 16 } else { 10 }).ok()?;
        return codepoint(cp).map(|ch| (ch, j + 1));
    }
    let start = j;
    while j < c.len() && (c[j].is_ascii_alphanumeric() || c[j] == '_') {
        j += 1;
    }
    if j == start || at(c, j) != ';' {
        return None;
    }
    let name = text_of(&c[start..j]);
    named_entity(&name)
        .and_then(|s| s.chars().next())
        .map(|ch| (ch, j + 1))
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

// --- Inline parsing helpers ---

fn closing_bracket(c: &[char], open: usize) -> Option<usize> {
    let mut depth: isize = 0;
    let mut i = open;
    while i < c.len() {
        match c[i] {
            '\\' => {
                i += 2;
                continue;
            }
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth <= 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn closing_paren(c: &[char], open: usize) -> Option<usize> {
    let mut depth: isize = 0;
    let mut i = open;
    while i < c.len() {
        match c[i] {
            '\\' => {
                i += 2;
                continue;
            }
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth <= 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// A backtick run at `i`, and the index just past its closer. Runs of any
/// length are honored, so a delimiter inside a multi-backtick span is inert.
fn skip_code_span(c: &[char], i: usize) -> Option<usize> {
    let mut n = 0;
    while i + n < c.len() && c[i + n] == '`' {
        n += 1;
    }
    find_from(c, i + n, &backticks(n)).map(|closer| closer + n)
}

fn find_delimiter(c: &[char], start: usize, delim: &str) -> Option<usize> {
    let mut i = start;
    while i < c.len() {
        if c[i] == '\\' {
            i += 2;
            continue;
        }
        if c[i] == '`' {
            if let Some(end) = skip_code_span(c, i) {
                i = end;
                continue;
            }
        }
        if starts(c, i, delim) {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn find_single_delimiter(c: &[char], start: usize, ch: char) -> Option<usize> {
    let mut i = start;
    while i < c.len() {
        if c[i] == '\\' {
            i += 2;
            continue;
        }
        if c[i] == '`' {
            if let Some(end) = skip_code_span(c, i) {
                i = end;
                continue;
            }
        }
        if c[i] == ch && (i == 0 || c[i - 1] != ch) && (i + 1 >= c.len() || c[i + 1] != ch) {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// A failed search from X fails from every position past X too, so one miss
/// per delimiter is enough: without this a line of unmatched `*` is quadratic.
fn cached(
    c: &[char],
    misses: &mut HashMap<&'static str, usize>,
    key: &'static str,
    start: usize,
    delim: &str,
) -> Option<usize> {
    if misses.get(key).is_some_and(|&m| start >= m) {
        return None;
    }
    let found = find_delimiter(c, start, delim);
    if found.is_none() {
        misses
            .entry(key)
            .and_modify(|m| *m = (*m).min(start))
            .or_insert(start);
    }
    found
}

fn cached_single(
    c: &[char],
    misses: &mut HashMap<&'static str, usize>,
    key: &'static str,
    start: usize,
    ch: char,
) -> Option<usize> {
    if misses.get(key).is_some_and(|&m| start >= m) {
        return None;
    }
    let found = find_single_delimiter(c, start, ch);
    if found.is_none() {
        misses
            .entry(key)
            .and_modify(|m| *m = (*m).min(start))
            .or_insert(start);
    }
    found
}

/// `<br>`, `<br/>`, `<br />`, any case.
fn br_tag(c: &[char], i: usize) -> Option<usize> {
    if at(c, i) != '<' {
        return None;
    }
    let mut j = i + 1;
    if !matches!(at(c, j), 'b' | 'B') || !matches!(at(c, j + 1), 'r' | 'R') {
        return None;
    }
    j += 2;
    while at(c, j).is_whitespace() {
        j += 1;
    }
    if at(c, j) == '/' {
        j += 1;
        while at(c, j).is_whitespace() {
            j += 1;
        }
    }
    if at(c, j) == '>' {
        Some(j + 1 - i)
    } else {
        None
    }
}

/// `https?:\/\/[^\s<>\[\]]*[^\s<>\[\].,;:!?)\]}'"']`, as a length.
fn autolink(c: &[char], i: usize) -> Option<usize> {
    let prefix = if starts(c, i, "https://") {
        8
    } else if starts(c, i, "http://") {
        7
    } else if starts(c, i, "www.") {
        4
    } else {
        return None;
    };
    let mut j = i + prefix;
    while j < c.len() && !c[j].is_whitespace() && !matches!(c[j], '<' | '>' | '[' | ']') {
        j += 1;
    }
    while j > i + prefix
        && matches!(
            c[j - 1],
            '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '\'' | '"'
        )
    {
        j -= 1;
    }
    if j > i + prefix {
        Some(j - i)
    } else {
        None
    }
}

fn flush_buf(buf: &mut String, out: &mut Vec<Inline>) {
    if !buf.is_empty() {
        let text = std::mem::take(buf);
        out.push(Inline::Text(if text.contains('&') {
            decode_entities(&text)
        } else {
            text
        }));
    }
}

fn closes_underscore(c: char) -> bool {
    c.is_whitespace() || matches!(c, '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}')
}

// --- Inline parser ---

fn parse_inline(text: &str) -> Vec<Inline> {
    inline(&as_chars(text), 0)
}

fn inline(t: &[char], depth: usize) -> Vec<Inline> {
    if t.is_empty() {
        return Vec::new();
    }
    if depth > MAX_NESTING_DEPTH {
        return vec![Inline::Text(text_of(t))];
    }
    let mut misses: HashMap<&'static str, usize> = HashMap::new();
    let last_bracket = t.iter().rposition(|&c| c == ']');
    let last_paren = t.iter().rposition(|&c| c == ')');
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut i = 0;

    while i < t.len() {
        let ch = t[i];

        // Escape
        if ch == '\\' && i + 1 < t.len() && "\\`*_~[]!|".contains(t[i + 1]) {
            buf.push(t[i + 1]);
            i += 2;
            continue;
        }

        // Hard line break: two or more spaces before a newline
        if ch == ' ' && at(t, i + 1) == ' ' {
            let mut j = i + 2;
            while j < t.len() && t[j] == ' ' {
                j += 1;
            }
            if at(t, j) == '\n' {
                flush_buf(&mut buf, &mut out);
                out.push(Inline::Br);
                i = j + 1;
                continue;
            }
        }

        // HTML <br>
        if ch == '<' {
            if let Some(len) = br_tag(t, i) {
                flush_buf(&mut buf, &mut out);
                out.push(Inline::Br);
                i += len;
                continue;
            }
        }

        // Image: ![alt](src)
        if ch == '!' && at(t, i + 1) == '[' && last_bracket.is_some_and(|b| b > i + 1) {
            if let Some(alt_end) = closing_bracket(t, i + 1) {
                if at(t, alt_end + 1) == '(' && last_paren.is_some_and(|p| p > alt_end) {
                    if let Some(src_end) = closing_paren(t, alt_end + 1) {
                        let alt = decode_entities(&text_of(&t[i + 2..alt_end]));
                        // Decoded before the check, so `&#106;avascript:`
                        // cannot slip through as a relative URL.
                        let src = decode_entities(&text_of(&t[alt_end + 2..src_end]));
                        flush_buf(&mut buf, &mut out);
                        if safe_url(&src) {
                            out.push(Inline::Image { src, alt });
                        } else {
                            buf.push_str(&text_of(&t[i..=src_end]));
                        }
                        i = src_end + 1;
                        continue;
                    }
                }
            }
        }

        // Link: [text](url)
        if ch == '[' && last_bracket.is_some_and(|b| b > i) {
            if let Some(text_end) = closing_bracket(t, i) {
                if at(t, text_end + 1) == '(' && last_paren.is_some_and(|p| p > text_end) {
                    if let Some(url_end) = closing_paren(t, text_end + 1) {
                        let label = text_of(&t[i + 1..text_end]);
                        let href = decode_entities(&text_of(&t[text_end + 2..url_end]));
                        flush_buf(&mut buf, &mut out);
                        if safe_url(&href) {
                            out.push(Inline::Link {
                                href,
                                children: inline(&as_chars(&label), depth + 1),
                            });
                        } else {
                            buf.push_str(&text_of(&t[i..=url_end]));
                        }
                        i = url_end + 1;
                        continue;
                    }
                }
            }
        }

        // Autolink: bare URL
        if (starts(t, i, "https://") || starts(t, i, "http://") || starts(t, i, "www."))
            && (i == 0 || " \t\n(".contains(t[i - 1]))
        {
            if let Some(len) = autolink(t, i) {
                flush_buf(&mut buf, &mut out);
                let url = text_of(&t[i..i + len]);
                let href = if url.starts_with("www.") {
                    format!("https://{url}")
                } else {
                    url.clone()
                };
                out.push(Inline::Link {
                    href: decode_entities(&href),
                    children: vec![Inline::Text(url)],
                });
                i += len;
                continue;
            }
        }

        // Code span
        if ch == '`' {
            let mut ticks = 0;
            let mut j = i;
            while j < t.len() && t[j] == '`' {
                ticks += 1;
                j += 1;
            }
            if let Some(closer) = find_from(t, j, &backticks(ticks)) {
                flush_buf(&mut buf, &mut out);
                let mut code = text_of(&t[j..closer]);
                if ticks > 1
                    && code.chars().count() > 1
                    && code.starts_with(' ')
                    && code.ends_with(' ')
                {
                    code = code.chars().skip(1).collect::<String>();
                    let n = code.chars().count();
                    code = code.chars().take(n - 1).collect();
                }
                out.push(Inline::Code(code));
                i = closer + ticks;
                continue;
            }
            // No closer: the backticks are text, and the run is stepped over
            // whole so its own first backtick is not scanned again.
            buf.push_str(&text_of(&t[i..j]));
            i = j;
            continue;
        }

        // Bold + italic: ***text***
        if starts(t, i, "***") {
            if let Some(end) = cached(t, &mut misses, "***", i + 3, "***") {
                flush_buf(&mut buf, &mut out);
                out.push(Inline::Strong(vec![Inline::Em(inline(
                    &t[i + 3..end],
                    depth + 1,
                ))]));
                i = end + 3;
                continue;
            }
        }

        // Bold: **text**
        if starts(t, i, "**") {
            if let Some(end) = cached(t, &mut misses, "**", i + 2, "**") {
                flush_buf(&mut buf, &mut out);
                out.push(Inline::Strong(inline(&t[i + 2..end], depth + 1)));
                i = end + 2;
                continue;
            }
        }

        // Strikethrough: ~~text~~
        if starts(t, i, "~~") {
            if let Some(end) = cached(t, &mut misses, "~~", i + 2, "~~") {
                flush_buf(&mut buf, &mut out);
                out.push(Inline::Del(inline(&t[i + 2..end], depth + 1)));
                i = end + 2;
                continue;
            }
        }

        // Strikethrough: ~text~
        if ch == '~' && !starts(t, i, "~~") {
            if let Some(end) = cached_single(t, &mut misses, "~", i + 1, '~') {
                flush_buf(&mut buf, &mut out);
                out.push(Inline::Del(inline(&t[i + 1..end], depth + 1)));
                i = end + 1;
                continue;
            }
        }

        // Italic: *text*
        if ch == '*' && !starts(t, i, "**") {
            if let Some(end) = cached_single(t, &mut misses, "~*", i + 1, '*') {
                flush_buf(&mut buf, &mut out);
                out.push(Inline::Em(inline(&t[i + 1..end], depth + 1)));
                i = end + 1;
                continue;
            }
        }

        // Italic: _text_ (only at a word boundary, so `a_b_c` stays literal)
        if ch == '_' && !starts(t, i, "__") && (i == 0 || !t[i - 1].is_ascii_alphanumeric()) {
            if let Some(end) = cached_single(t, &mut misses, "~_", i + 1, '_') {
                if end + 1 >= t.len() || closes_underscore(t[end + 1]) {
                    flush_buf(&mut buf, &mut out);
                    out.push(Inline::Em(inline(&t[i + 1..end], depth + 1)));
                    i = end + 1;
                    continue;
                }
            }
        }

        buf.push(ch);
        i += 1;
    }

    flush_buf(&mut buf, &mut out);
    out
}

// --- Block helpers ---

fn parse_cells(line: &str) -> Vec<String> {
    let mut s = line;
    if let Some(rest) = s.strip_prefix('|') {
        s = rest;
    }
    if let Some(rest) = s.strip_suffix('|') {
        s = rest;
    }
    let c = as_chars(s);
    let mut cells = Vec::new();
    let mut buf = String::new();
    let mut i = 0;
    while i < c.len() {
        if c[i] == '\\' && at(&c, i + 1) == '|' {
            buf.push('|');
            i += 2;
        } else if c[i] == '|' {
            cells.push(buf.trim().to_string());
            buf.clear();
            i += 1;
        } else {
            buf.push(c[i]);
            i += 1;
        }
    }
    cells.push(buf.trim().to_string());
    cells
}

fn parse_alignments(line: &str) -> Vec<Option<Align>> {
    parse_cells(line)
        .iter()
        .map(|cell| {
            let left = cell.starts_with(':');
            let right = cell.ends_with(':');
            match (left, right) {
                (true, true) => Some(Align::Center),
                (false, true) => Some(Align::Right),
                (true, false) => Some(Align::Left),
                _ => None,
            }
        })
        .collect()
}

/// One cell of `|---|:--:|--:|---|`.
fn is_delim_cell(s: &str) -> bool {
    let c = as_chars(s);
    if c.is_empty() {
        return false;
    }
    let left = c[0] == ':';
    let right = c.len() > 1 && c[c.len() - 1] == ':';
    let start = usize::from(left);
    let end = c.len() - usize::from(right);
    if end <= start {
        return false;
    }
    let dashes = &c[start..end];
    if !dashes.iter().all(|&ch| ch == '-') {
        return false;
    }
    match (left, right) {
        (false, false) => dashes.len() >= 3,
        (true, true) => !dashes.is_empty(),
        _ => dashes.len() >= 2,
    }
}

fn is_table_delim(line: &str) -> bool {
    let mut s = line.trim();
    if let Some(rest) = s.strip_prefix('|') {
        s = rest;
    }
    if let Some(rest) = s.strip_suffix('|') {
        s = rest;
    }
    s.split('|').all(|cell| is_delim_cell(cell.trim()))
}

fn fence_open(line: &str) -> Option<(char, usize, String)> {
    let t = line.trim();
    let ch = t.chars().next()?;
    if ch != '`' && ch != '~' {
        return None;
    }
    let n = t.chars().take_while(|&c| c == ch).count();
    if n < 3 {
        return None;
    }
    let lang = t.chars().skip(n).collect::<String>().trim().to_string();
    Some((ch, n, lang))
}

fn fence_closes(line: &str, ch: char, n: usize) -> bool {
    let t = line.trim();
    let count = t.chars().take_while(|&c| c == ch).count();
    count >= n && t.chars().all(|c| c == ch)
}

fn is_hr(t: &str) -> bool {
    let mut marks = 0;
    for c in t.chars() {
        if c.is_whitespace() {
            continue;
        }
        if !matches!(c, '-' | '*' | '_') {
            return false;
        }
        marks += 1;
    }
    marks >= 3
}

fn heading_start(t: &str) -> bool {
    let level = t.chars().take_while(|&c| c == '#').count();
    (1..=6).contains(&level) && t[level..].chars().next().is_some_and(char::is_whitespace)
}

fn heading(line: &str) -> Option<(usize, Vec<Inline>)> {
    let t = line.trim();
    if !heading_start(t) {
        return None;
    }
    let level = t.chars().take_while(|&c| c == '#').count();
    let rest = t[level..].trim();
    // Closing hashes are decoration, not content: `## title ##`.
    let mut content = rest;
    if content.ends_with('#') {
        let bare = content.trim_end_matches('#');
        if bare.ends_with(char::is_whitespace) {
            content = bare.trim_end();
        }
    }
    Some((level, parse_inline(content)))
}

fn unordered_marker(t: &str) -> bool {
    let b = t.as_bytes();
    b.len() > 1 && matches!(b[0], b'-' | b'*' | b'+') && b[1] == b' '
}

fn ordered_marker(t: &str) -> bool {
    let digits = t.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits == 0 {
        return false;
    }
    let mut rest = t[digits..].chars();
    matches!(rest.next(), Some('.' | ')')) && rest.next() == Some(' ')
}

fn details_start(t: &str) -> bool {
    find_ci(t, "<details").is_some_and(|at| {
        at == 0
            && t["<details".len()..]
                .chars()
                .next()
                .is_some_and(|c| c.is_whitespace() || c == '>')
    })
}

fn details_end(line: &str) -> bool {
    find_ci(line, "</details")
        .is_some_and(|at| line[at + "</details".len()..].trim_start().starts_with('>'))
}

fn find_ci(hay: &str, needle: &str) -> Option<usize> {
    let n = needle.len();
    let h = hay.as_bytes();
    if n == 0 || h.len() < n {
        return None;
    }
    (0..=h.len() - n).find(|&i| h[i..i + n].eq_ignore_ascii_case(needle.as_bytes()))
}

fn strip_tag(text: &str, tag: &str) -> String {
    let open = format!("<{tag}");
    let close = format!("</{tag}");
    let mut s = text.to_string();
    loop {
        let found = match (find_ci(&s, &open), find_ci(&s, &close)) {
            (Some(a), Some(b)) => a.min(b),
            (Some(a), None) => a,
            (None, Some(b)) => b,
            (None, None) => return s,
        };
        match s[found..].find('>') {
            Some(gt) => s.replace_range(found..found + gt + 1, ""),
            None => return s,
        }
    }
}

fn summary_of(raw: &str) -> String {
    let Some(open) = find_ci(raw, "<summary") else {
        return String::new();
    };
    let Some(gt) = raw[open..].find('>') else {
        return String::new();
    };
    let start = open + gt + 1;
    let Some(close) = find_ci(&raw[start..], "</summary") else {
        return String::new();
    };
    raw[start..start + close].trim().to_string()
}

fn strip_details(raw: &str) -> String {
    let s = strip_tag(raw, "details");
    let Some(open) = find_ci(&s, "<summary") else {
        return s.trim().to_string();
    };
    let Some(gt) = s[open..].find('>') else {
        return s.trim().to_string();
    };
    let after = open + gt + 1;
    let Some(close) = find_ci(&s[after..], "</summary") else {
        return s.trim().to_string();
    };
    let close = after + close;
    match s[close..].find('>') {
        Some(gt) => format!("{}{}", &s[..open], &s[close + gt + 1..])
            .trim()
            .to_string(),
        None => s.trim().to_string(),
    }
}

/// `(indent, marker, content)` for a list item line.
fn list_item(line: &str) -> Option<(usize, String, &str)> {
    let indent = line.chars().take_while(|c| matches!(c, ' ' | '\t')).count();
    let rest = &line[indent..];
    let marker = match rest.chars().next()? {
        c @ ('-' | '*' | '+') => c.to_string(),
        _ => {
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if digits.is_empty() {
                return None;
            }
            match rest[digits.len()..].chars().next()? {
                c @ ('.' | ')') => format!("{digits}{c}"),
                _ => return None,
            }
        }
    };
    let content = rest[marker.len()..].strip_prefix(' ')?;
    Some((indent, marker, content))
}

fn is_block_start(line: &str, next: Option<&str>) -> bool {
    let t = line.trim();
    if heading_start(t)
        || is_hr(t)
        || t.starts_with("```")
        || t.starts_with("~~~")
        || t.starts_with('>')
        || unordered_marker(t)
        || ordered_marker(t)
        || details_start(t)
    {
        return true;
    }
    t.starts_with('|') && next.is_some_and(|n| is_table_delim(n.trim()))
}

// --- Block parser ---

fn parse_blocks(input: &str, depth: usize) -> Vec<Block> {
    if depth > MAX_NESTING_DEPTH {
        return if input.is_empty() {
            Vec::new()
        } else {
            vec![Block::Paragraph(parse_inline(input))]
        };
    }
    let lines: Vec<&str> = input.split('\n').collect();
    let mut blocks = Vec::new();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim();

        if trimmed.is_empty() {
            i += 1;
            continue;
        }

        // Fenced code block
        if let Some((ch, n, lang)) = fence_open(line) {
            let mut code: Vec<&str> = Vec::new();
            i += 1;
            while i < lines.len() {
                if fence_closes(lines[i], ch, n) {
                    i += 1;
                    break;
                }
                code.push(lines[i]);
                i += 1;
            }
            blocks.push(Block::Code {
                lang,
                content: code.join("\n"),
            });
            continue;
        }

        // ATX heading
        if let Some((level, content)) = heading(line) {
            blocks.push(Block::Heading { level, content });
            i += 1;
            continue;
        }

        // Horizontal rule
        if is_hr(trimmed) {
            blocks.push(Block::Hr);
            i += 1;
            continue;
        }

        // Table
        if trimmed.contains('|') && i + 1 < lines.len() {
            let headers = parse_cells(line);
            let aligns = parse_alignments(lines[i + 1]);
            if headers.len() == aligns.len() && is_table_delim(lines[i + 1]) {
                let headers = headers
                    .iter()
                    .take(MAX_TABLE_COLUMNS)
                    .map(|c| parse_inline(c))
                    .collect();
                let aligns = aligns.into_iter().take(MAX_TABLE_COLUMNS).collect();
                let mut rows: Vec<Vec<Vec<Inline>>> = Vec::new();
                i += 2;
                while i < lines.len()
                    && !lines[i].trim().is_empty()
                    && lines[i].contains('|')
                    && rows.len() < MAX_TABLE_ROWS
                {
                    rows.push(
                        parse_cells(lines[i])
                            .iter()
                            .take(MAX_TABLE_COLUMNS)
                            .map(|c| parse_inline(c))
                            .collect(),
                    );
                    i += 1;
                }
                blocks.push(Block::Table {
                    headers,
                    aligns,
                    rows,
                });
                continue;
            }
        }

        // Blockquote
        if trimmed.starts_with('>') {
            let mut quoted: Vec<String> = Vec::new();
            while i < lines.len() {
                let t = lines[i].trim();
                if let Some(rest) = t.strip_prefix("> ") {
                    quoted.push(rest.to_string());
                } else if t == ">" {
                    quoted.push(String::new());
                } else if let Some(rest) = t.strip_prefix('>') {
                    quoted.push(rest.to_string());
                } else {
                    break;
                }
                i += 1;
            }
            blocks.push(Block::Blockquote(parse_blocks(
                &quoted.join("\n"),
                depth + 1,
            )));
            continue;
        }

        // List
        if let Some((base_indent, first, _)) = list_item(line) {
            let ordered = first.chars().next().is_some_and(|c| c.is_ascii_digit());
            let start: usize = if ordered {
                first.trim_end_matches(['.', ')']).parse().unwrap_or(1)
            } else {
                1
            };
            let marker_len = first.chars().count() + 1;
            let mut items: Vec<Vec<Block>> = Vec::new();
            let mut current: Vec<String> = Vec::new();

            while i < lines.len() && items.len() < MAX_LIST_ITEMS {
                let li = lines[i];
                match list_item(li) {
                    Some((indent, _, content)) if indent == base_indent => {
                        if !current.is_empty() {
                            items.push(parse_blocks(&current.join("\n"), depth + 1));
                        }
                        current = vec![content.to_string()];
                        i += 1;
                    }
                    // A blank line inside an item is a loose list, not its end,
                    // as long as another item follows.
                    _ if li.trim().is_empty() => {
                        let next_item = lines
                            .get(i + 1)
                            .and_then(|l| list_item(l))
                            .is_some_and(|(indent, _, _)| indent == base_indent);
                        if next_item {
                            current.push(String::new());
                            i += 1;
                        } else {
                            break;
                        }
                    }
                    _ => {
                        let lead = li.chars().take_while(|c| c.is_whitespace()).count();
                        if li.chars().count() > base_indent + marker_len
                            && lead > 0
                            && lead >= base_indent + marker_len
                        {
                            current.push(li.chars().skip(base_indent + marker_len).collect());
                            i += 1;
                        } else {
                            break;
                        }
                    }
                }
            }
            if !current.is_empty() && items.len() < MAX_LIST_ITEMS {
                items.push(parse_blocks(&current.join("\n"), depth + 1));
            }
            blocks.push(Block::List {
                ordered,
                start,
                items,
            });
            continue;
        }

        // Details/Summary
        if details_start(trimmed) {
            let mut raw = vec![trimmed.to_string()];
            i += 1;
            while i < lines.len() {
                raw.push(lines[i].to_string());
                let last = details_end(lines[i]);
                i += 1;
                if last {
                    break;
                }
            }
            let raw = raw.join("\n");
            blocks.push(Block::Details {
                summary: parse_inline(&summary_of(&raw)),
                blocks: parse_blocks(strip_details(&raw).trim(), depth + 1),
            });
            continue;
        }

        // Paragraph
        let mut para: Vec<&str> = Vec::new();
        while i < lines.len()
            && !lines[i].trim().is_empty()
            && !is_block_start(lines[i], lines.get(i + 1).copied())
        {
            para.push(lines[i]);
            i += 1;
        }
        if para.is_empty() {
            i += 1;
        } else {
            blocks.push(Block::Paragraph(parse_inline(&para.join("\n"))));
        }
    }

    blocks
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
    render(&parse_blocks(text, 0), width, &mut out);
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
            Block::Code { lang, content } => {
                let _ = lang;
                for line in content.split('\n') {
                    let mut spans = gutter.to_vec();
                    spans.push(Span::styled(line.to_string(), base.patch(CODE)));
                    out.push(Line::from(spans));
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
            Block::Details { summary, blocks } => {
                push_rows(out, gutter, wrap(&pieces(summary, base.patch(BOLD)), inner));
                emit(blocks, width, base, gutter, out);
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
/// they belong to ends: at a blank line, or when the caller flushes because
/// something else goes into the transcript. A fenced block is opaque, so its
/// blank lines are content, not boundaries.
#[derive(Default)]
pub struct Stream {
    lines: Vec<String>,
    fence: Option<(char, usize)>,
}

impl Stream {
    pub fn new() -> Stream {
        Stream::default()
    }

    pub fn push(&mut self, line: &str, width: usize, out: &mut Vec<Line<'static>>) {
        if let Some((ch, n)) = self.fence {
            self.lines.push(line.to_string());
            if fence_closes(line, ch, n) {
                self.fence = None;
            }
            return;
        }
        if let Some((ch, n, _)) = fence_open(line) {
            self.fence = Some((ch, n));
            self.lines.push(line.to_string());
            return;
        }
        if line.trim().is_empty() {
            self.flush(width, out);
            return;
        }
        self.lines.push(line.to_string());
    }

    pub fn flush(&mut self, width: usize, out: &mut Vec<Line<'static>>) {
        if self.lines.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.lines).join("\n");
        self.fence = None;
        render(&parse_blocks(&text, 0), width, out);
    }
}

// --- Output for a stream that has no screen ---

fn plain(line: &Line) -> String {
    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    text.trim_end().to_string()
}

pub fn ansi(line: &Line) -> String {
    if !io::stdout().is_terminal() {
        return plain(line);
    }
    let mut out = String::new();
    for span in &line.spans {
        let codes = ansi_codes(span.style);
        if codes.is_empty() {
            out.push_str(&span.content);
        } else {
            out.push_str(&format!("\x1b[{}m{}\x1b[0m", codes.join(";"), span.content));
        }
    }
    out.trim_end().to_string()
}

fn ansi_codes(style: Style) -> Vec<&'static str> {
    let mut codes = Vec::new();
    match style.fg {
        Some(Color::Cyan) => codes.push("36"),
        Some(Color::Green) => codes.push("32"),
        Some(Color::Yellow) => codes.push("33"),
        Some(Color::Red) => codes.push("31"),
        _ => {}
    }
    for (bit, code) in [
        (Modifier::BOLD, "1"),
        (Modifier::DIM, "2"),
        (Modifier::ITALIC, "3"),
        (Modifier::UNDERLINED, "4"),
        (Modifier::CROSSED_OUT, "9"),
    ] {
        if style.add_modifier.contains(bit) {
            codes.push(code);
        }
    }
    codes
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
}
