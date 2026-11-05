//! Transcript text: what a message, a tool call and a command look like on one
//! line. The screen renders the same spans as cells, so the only difference
//! between the two outputs is who does the painting.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;

use crate::api;

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
}

impl Paint {
    pub fn style(self) -> Style {
        match self {
            Paint::Plain => Style::new(),
            Paint::Dim => Style::new().add_modifier(Modifier::DIM),
            Paint::Cyan => Style::new().fg(Color::Cyan),
            Paint::Yellow => Style::new().fg(Color::Yellow),
            Paint::Green => Style::new().fg(Color::Green),
            Paint::Red => Style::new().fg(Color::Red),
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
        (paint, summarize(command, 120)),
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
            parts.push(summarize(s, 120));
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

    let mut spans = vec![
        (Paint::Plain, "  ".to_string()),
        (paint, glyph.to_string()),
        (Paint::Plain, " ".to_string()),
        (paint, call.name.clone()),
    ];
    if parts.is_empty() {
        return spans;
    }
    spans.push((Paint::Plain, format!("({})", parts.join(" "))));
    spans
}

/// A failed tool result is a result, not an error: tools report failures as
/// text, either `error: ...` or a non-zero `exit: N`.
pub(crate) fn failed(result: &str) -> bool {
    if let Some(rest) = result.strip_prefix("exit: ") {
        rest.lines().next().unwrap_or("").trim() != "0"
    } else {
        result.starts_with("error:")
    }
}

/// One glyph for the outcome.
pub(crate) fn mark(result: &str) -> (Paint, &'static str) {
    if failed(result) {
        (Paint::Red, "✗")
    } else {
        (Paint::Green, "✓")
    }
}

/// What to say under a failed call: one line. The `exit: N` header is dropped,
/// since the mark already reports the failure. The full result still reaches
/// the model; a build log must not reach the transcript.
pub(crate) fn detail(result: &str) -> String {
    let body = match result.strip_prefix("exit: ") {
        Some(rest) => rest.split_once('\n').map(|(_, body)| body).unwrap_or(""),
        None => result,
    };
    summarize(body.trim_start_matches('\n'), 120)
}

pub(crate) fn summarize(text: &str, max: usize) -> String {
    let line = text.lines().next().unwrap_or("");
    let mut out: String = line.chars().take(max).collect();
    if line.chars().count() > max || text.lines().count() > 1 {
        out.push('…');
    }
    out
}
