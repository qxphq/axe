//! Transcript text: what a message, a tool call and a command look like on one
//! line. The screen renders the same spans as cells, so the only difference
//! between the two outputs is who does the painting.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;

use crate::api;
use crate::text;

/// A style this binary uses for transcript text. Two renderers read it: the
/// screen's cells, and ANSI escapes when there is no screen.
#[derive(Clone, Copy)]
pub enum Paint {
    Plain,
    Dim,
    Cyan,
    Yellow,
    Green,
    Red,
    Magenta,
}

impl Paint {
    pub const fn style(self) -> Style {
        match self {
            Paint::Plain => Style::new(),
            Paint::Dim => Style::new().add_modifier(Modifier::DIM),
            Paint::Cyan => Style::new().fg(Color::Cyan),
            Paint::Yellow => Style::new().fg(Color::Yellow),
            Paint::Green => Style::new().fg(Color::Green),
            Paint::Red => Style::new().fg(Color::Red),
            Paint::Magenta => Style::new().fg(Color::Magenta),
        }
    }

    /// The span as ANSI, for the paths that have no screen. `text::ansi` owns
    /// the codes: this says what the text is, the terminal's names say how it
    /// looks.
    pub fn ansi(self, text: &str) -> String {
        crate::text::ansi(text, self.style())
    }

    pub fn span(self, text: String) -> Span<'static> {
        Span::styled(text, self.style())
    }
}

pub(crate) fn line_of(spans: &[(Paint, String)]) -> Line<'static> {
    Line::from(
        spans
            .iter()
            .map(|(paint, text)| paint.span(text.clone()))
            .collect::<Vec<_>>(),
    )
}

pub(crate) fn plain_text(spans: &[(Paint, String)]) -> String {
    spans
        .iter()
        .map(|(_, text)| text.as_str())
        .collect::<String>()
        .trim()
        .to_string()
}

// A shell command, the user's (`!cmd`) or the model's, on one line: the glyph
// says what runs, and the command follows it, since `bash` has no call syntax.
pub(crate) fn shell_line(command: &str, explore: bool) -> Vec<(Paint, String)> {
    // Cyan for a command that cannot change the tree: dim is the thought's
    // color, and the two must not read alike.
    let paint = if explore { Paint::Cyan } else { Paint::Yellow };
    vec![
        (Paint::Plain, "  ".to_string()),
        (paint, "▷".to_string()),
        (Paint::Plain, " ".to_string()),
        (paint, text::oneline(command, 120)),
    ]
}

// Echo tool calls compactly: show the arguments that identify the target, never
// the raw JSON (heredoc scripts and long content are unreadable in a line).
// A glyph per tool says what it does at a glance. The palette has one rule:
// anything that can change the tree is yellow, anything that cannot is cyan
// (dim is the thought's color, so a read-only call must not use it).
pub(crate) fn call_line(call: &api::ToolCall, explore: bool) -> Vec<(Paint, String)> {
    let v: Value = serde_json::from_str(&call.arguments).unwrap_or(Value::Null);
    let (glyph, paint) = match call.name.as_str() {
        "read" => ("◇", Paint::Cyan),
        "fetch" => ("↗", Paint::Cyan),
        "web_search" => ("⌕", Paint::Cyan),
        "about" => ("ℹ", Paint::Cyan),
        "write" => ("✚", Paint::Yellow),
        "edit" => ("✎", Paint::Yellow),
        _ => ("✚", Paint::Yellow),
    };

    if call.name == "bash" {
        let command = v["command"].as_str().unwrap_or("bash");
        return shell_line(command, explore);
    }

    let mut parts = Vec::new();
    for key in ["path", "file", "pattern", "url", "query"] {
        if let Some(s) = v[key].as_str() {
            parts.push(text::oneline(s, 120));
        }
    }
    // A partial read is only reproducible with its window, so echo it. `fetch`
    // pages with start/chars; `read` with offset/limit.
    let num = |key: &str| v[key].as_u64().map(|n| n.to_string());
    let (offset, limit) = (
        num("offset").or_else(|| num("start")),
        num("limit").or_else(|| num("chars")),
    );
    if offset.is_some() || limit.is_some() {
        let start = offset.unwrap_or_else(|| "1".into());
        parts.push(match limit {
            Some(l) => format!("{start}+{l}"),
            None => format!("{start}+"),
        });
    }
    // An edit's path says nothing about its size: old→new lines tells a tweak
    // from a rewrite, the way read's window says which slice it took.
    if call.name == "edit" {
        if let (Some(old), Some(new)) = (v["old"].as_str(), v["new"].as_str()) {
            parts.push(format!(
                "{}→{} lines",
                old.lines().count(),
                new.lines().count()
            ));
        }
    }

    // `ℹ` is missing from most text fonts, so the terminal falls back to a wide
    // glyph whose ink covers the cell the line reserves after it and the name
    // reads glued to it. A second space is a cell for the overflow to cover: the
    // line is one cell wider than a narrow `ℹ` needs, which is the cheaper
    // fault. No other glyph in the palette draws wider than its advance.
    let gap = if call.name == "about" { "  " } else { " " };
    let mut spans = vec![
        (Paint::Plain, "  ".to_string()),
        (paint, glyph.to_string()),
        (Paint::Plain, gap.to_string()),
        (paint, call.name.clone()),
    ];
    if parts.is_empty() {
        return spans;
    }
    spans.push((Paint::Plain, format!("({})", parts.join(" "))));
    spans
}

/// A failed tool result is a result, not an error: tools report failures as
/// text, either `error: ...` or a non-zero `exit: N`. `read` is the one tool
/// whose success is arbitrary file content, so a file whose first line begins
/// `error:` must not read as its own failure: what identifies a `read` failure
/// is one of the shapes the tool itself writes — `error: read ...`,
/// `error: missing "..."` and `error: "<name>" must be ...`. A file whose first
/// line is one of those exact shapes is still misjudged, and no header could
/// tell that apart without changing the content the model needs.
pub(crate) fn failed(name: &str, result: &str) -> bool {
    if let Some(rest) = result.strip_prefix("exit: ") {
        return rest.lines().next().unwrap_or("").trim() != "0";
    }
    if name == "read" {
        return result.starts_with("error: read ")
            || result.starts_with("error: missing \"path\"")
            || result.starts_with("error: \"");
    }
    result.starts_with("error:")
}

/// One glyph for the outcome.
pub(crate) fn mark(name: &str, result: &str) -> (Paint, &'static str) {
    if failed(name, result) {
        (Paint::Red, "✗")
    } else {
        (Paint::Green, "✓")
    }
}

/// A finished command on one line: what ran, and one glyph for how it went.
/// The model's calls and the user's `!cmd` share it, so a command reads the
/// same whichever side ran it.
pub(crate) fn shell_echo(command: &str, result: &str) -> Vec<(Paint, String)> {
    let mut spans = shell_line(command, false);
    spans.push((Paint::Plain, " ".to_string()));
    let (paint, glyph) = mark("bash", result);
    spans.push((paint, glyph.to_string()));
    spans
}

/// A tool result's own words: the `exit: N` header is dropped, since the glyph
/// on the call's line already reports the outcome.
pub(crate) fn body(result: &str) -> &str {
    match result.strip_prefix("exit: ") {
        Some(rest) => rest.split_once('\n').map(|(_, body)| body).unwrap_or(""),
        None => result,
    }
}

/// What to say under a failed call: one line. The `exit: N` header is dropped,
/// since the mark already reports the failure. The full result still reaches
/// the model; a build log must not reach the transcript.
pub(crate) fn detail(result: &str) -> String {
    text::oneline(body(result).trim_start_matches('\n'), 120)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_read_of_content_that_looks_like_an_error_succeeded() {
        // The file's own words, not the tool's: a log, a parser fixture.
        assert!(!failed("read", "error: the box is empty"));
        assert!(!failed("read", "error: missing semicolon at line 3"));
        assert_eq!(mark("read", "error: nope").1, "✓");
        // The tool's own failures still read as ones.
        assert!(failed("read", "error: read src/x.rs: not a regular file"));
        assert!(failed(
            "read",
            "error: read src/x.rs: offset 9 is past the end (3 lines)"
        ));
        assert!(failed("read", "error: missing \"path\""));
        assert!(failed(
            "read",
            "error: \"offset\" must be a non-negative integer, got \"12\""
        ));
        assert_eq!(mark("read", "error: read x: gone").1, "✗");
    }

    #[test]
    fn every_other_tool_keeps_the_plain_rule() {
        assert!(failed("edit", "error: edit a: text not found"));
        assert!(!failed("about", "# axe"));
        assert!(!failed("bash", "exit: 0\nok"));
        assert!(failed("bash", "exit: 1\nboom"));
        assert!(failed("bash", "exit: signal\n"));
        assert_eq!(mark("write", "wrote 3 bytes to a").1, "✓");
    }
}
