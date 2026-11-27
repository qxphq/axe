//! Transcript text: what a message, a tool call and a command look like. The screen renders the same spans as cells, so the only difference
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

    /// The same, for a line written to stderr: styled and stripped when stderr
    /// is a terminal, whatever stdout is.
    pub fn ansi_err(self, text: &str) -> String {
        crate::text::ansi_err(text, self.style())
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

/// The most rows a finished command takes: a script reads whole, and a file
/// written through a heredoc does not flood the scrollback.
const SHELL_ROWS: usize = 200;

/// One row of styled spans.
pub(crate) type Row = Vec<(Paint, String)>;

// The shell glyph and its color: whether the command could change the tree.
// Cyan for one that could not — dim is the thought's color, and the two must
// not read alike — yellow for one that could, and plain when the mode it ran
// under is not known: a resumed session's calls, since the log keeps no mode.
fn shell_head(text: String, explore: Option<bool>) -> (Row, Paint) {
    let paint = match explore {
        Some(true) => Paint::Cyan,
        Some(false) => Paint::Yellow,
        None => Paint::Plain,
    };
    let row = vec![
        (Paint::Plain, "  ".to_string()),
        (paint, "▷".to_string()),
        (Paint::Plain, " ".to_string()),
        (paint, text),
    ];
    (row, paint)
}

// A shell command, the user's (`!cmd`) or the model's, on the status row's one
// line: the glyph says what runs, and the command follows it, since `bash` has
// no call syntax. A leading blank line would leave the row with no command.
pub(crate) fn shell_line(command: &str, explore: bool) -> Row {
    shell_head(text::oneline(command.trim_start(), 120), Some(explore)).0
}

/// A finished command as rows: the glyph and its first line, then the lines
/// after it behind a dim `│`, which sets them apart from a failure's words
/// under them. Blank lines are left out: in a tool block a blank row reads as
/// the end of the block. Past `SHELL_ROWS` the rest is counted, not shown.
pub(crate) fn shell_rows(command: &str, explore: Option<bool>) -> Vec<Row> {
    let mut lines = command
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| text::displayable(&line.replace('\t', "    ")));
    let (head, paint) = shell_head(lines.next().unwrap_or_default(), explore);
    let rest: Vec<String> = lines.collect();
    let row = |paint, line| {
        vec![
            (Paint::Plain, "  ".to_string()),
            (Paint::Dim, "│".to_string()),
            (Paint::Plain, " ".to_string()),
            (paint, line),
        ]
    };
    let room = SHELL_ROWS - 1;
    let shown = if rest.len() > room {
        room - 1
    } else {
        rest.len()
    };
    let mut rows = vec![head];
    rows.extend(rest[..shown].iter().map(|line| row(paint, line.clone())));
    if shown < rest.len() {
        rows.push(row(Paint::Dim, format!("… +{} lines", rest.len() - shown)));
    }
    rows
}

/// A finished call as rows: a `bash` command whole, any other call on its one
/// line. `explore` is `None` when the mode the call ran under is not known.
pub(crate) fn call_rows(call: &api::ToolCall, explore: Option<bool>) -> Vec<Row> {
    if call.name == "bash" {
        let v: Value = serde_json::from_str(&call.arguments).unwrap_or(Value::Null);
        if let Some(command) = v["command"].as_str() {
            return shell_rows(command, explore);
        }
    }
    vec![call_line(call, explore.unwrap_or(false))]
}

/// The outcome's glyph at the end of a call's first row, where a one-line
/// call carries it, so the marks stay in one column of the eye.
pub(crate) fn marked(mut rows: Vec<Row>, mark: (Paint, &str)) -> Vec<Row> {
    if let Some(first) = rows.first_mut() {
        first.push((Paint::Plain, " ".to_string()));
        first.push((mark.0, mark.1.to_string()));
    }
    rows
}

/// Rows as plain lines for a terminal with no screen: the first row's indent
/// dropped, as `plain_text` drops it, and the rest kept relative to it.
pub(crate) fn plain_rows(rows: &[Row]) -> String {
    rows.iter()
        .map(|row| {
            let line: String = row.iter().map(|(_, text)| text.as_str()).collect();
            line.strip_prefix("  ")
                .unwrap_or(&line)
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// Echo tool calls compactly: show the arguments that identify the target, never
// the raw JSON. `bash` is the exception that proves it: its argument is the
// call, and `call_rows` shows it whole.
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

    // `ℹ` and `✚` are missing from most text fonts, so the terminal falls back
    // to a wide glyph whose ink covers the cell the line reserves after it and
    // the name reads glued to it. A second space is a cell for the overflow to
    // cover: the line is one cell wider than a narrow glyph needs, which is the
    // cheaper fault.
    let gap = if matches!(glyph, "ℹ" | "✚") {
        "  "
    } else {
        " "
    };
    let mut spans = vec![
        (Paint::Plain, "  ".to_string()),
        (paint, glyph.to_string()),
        (Paint::Plain, gap.to_string()),
        // The name is the model's to choose, like the arguments: an unknown
        // one is still echoed, so it is cleaned the same way.
        (paint, text::oneline(&call.name, 64)),
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

/// A finished command: what ran, and one glyph for how it went. The model's
/// calls and the user's `!cmd` share it, so a command reads the same whichever
/// side ran it.
pub(crate) fn shell_echo(command: &str, result: &str) -> Vec<Row> {
    marked(shell_rows(command, Some(false)), mark("bash", result))
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

    /// A tool name is the model's: one carrying a clipboard or title escape is
    /// echoed without its control bytes, wherever the line is written.
    /// Blank lines leave no row: a command that starts with one still shows
    /// a command on its first row, and none reads as the end of the block.
    #[test]
    fn a_command_echoes_no_blank_rows() {
        let rows = shell_rows("\n  \ncd src\n\nmake\n", Some(false));
        assert_eq!(plain_rows(&rows), "▷ cd src\n│ make");
        assert_eq!(
            plain_text(&shell_line("\ncd src\nmake", false)),
            "▷ cd src…"
        );
    }

    /// A resumed call's mode is not in the log: its command is plain, not the
    /// color of a confinement it may not have run under.
    #[test]
    fn a_call_of_unknown_mode_is_plain() {
        let call = api::ToolCall {
            id: "1".into(),
            name: "bash".into(),
            arguments: r#"{"command":"make\nmake test"}"#.into(),
        };
        let rows = call_rows(&call, None);
        assert!(rows.iter().all(|row| matches!(row[3].0, Paint::Plain)));
        assert!(matches!(call_rows(&call, Some(true))[0][3].0, Paint::Cyan));
        assert!(matches!(
            call_rows(&call, Some(false))[1][3].0,
            Paint::Yellow
        ));
    }

    /// A heredoc that writes a big file takes `SHELL_ROWS` rows, and the last
    /// one counts what it left out.
    #[test]
    fn a_long_command_is_capped_and_counted() {
        let script: Vec<String> = (0..1000).map(|n| format!("line {n}")).collect();
        let rows = shell_rows(&script.join("\n"), Some(false));
        assert_eq!(rows.len(), SHELL_ROWS);
        let last = plain_rows(&rows[SHELL_ROWS - 1..]);
        assert_eq!(last, format!("│ … +{} lines", 1000 - (SHELL_ROWS - 1)));
        let exact: Vec<String> = (0..SHELL_ROWS).map(|n| format!("l{n}")).collect();
        let rows = shell_rows(&exact.join("\n"), Some(false));
        assert_eq!(rows.len(), SHELL_ROWS);
        assert_eq!(
            plain_rows(&rows[SHELL_ROWS - 1..]),
            format!("│ l{}", SHELL_ROWS - 1)
        );
    }

    #[test]
    fn a_made_up_tool_name_carries_no_control_bytes() {
        let call = api::ToolCall {
            id: "c".into(),
            name: "\x1b]52;c;aGk=\x07evil".into(),
            arguments: "{}".into(),
        };
        let line: String = call_line(&call, false)
            .into_iter()
            .map(|(_, text)| text)
            .collect();
        assert!(!line.chars().any(char::is_control), "{line:?}");
        assert!(line.contains("evil"), "{line:?}");
        // A stderr line strips what it styles, decided by stderr.
        let styled = text::styled("\x1b]0;title\x07x", Paint::Red.style());
        assert_eq!(styled, "\x1b[31m]0;titlex\x1b[0m", "only axe's own codes");
    }

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
