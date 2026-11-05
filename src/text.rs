//! Text the harness handles itself: bytes that came from somewhere else.
//!
//! A fetched page, a command's stderr and the model's own prose all reach the
//! terminal through here. Three things are the harness's, not the content's:
//! control bytes (a page can carry an escape sequence), the output cap (a
//! command can print a gigabyte) and the style (a `Style` becomes SGR in one
//! place, so the palette is one table).

use std::io::{self, IsTerminal};

use ratatui::style::{Color, Modifier, Style};

/// Drop control bytes, keeping the two that lay text out. Content is never
/// trusted to be text: a page that carries ESC would otherwise paint.
pub fn strip_control(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect()
}

/// Text for the terminal: stripped when there is one to strip for. Redirected
/// output keeps its bytes, so a pipe is the same text the page held.
pub fn terminal_text(text: &str) -> String {
    if io::stdout().is_terminal() {
        strip_control(text)
    } else {
        text.to_string()
    }
}

/// Keep both ends of an oversized result: the error is at the end and the
/// command is at the start, and the model can tell what was cut.
pub fn truncate(text: String, max: usize) -> String {
    let total = text.chars().count();
    if total <= max {
        return text;
    }
    let marker = "\n...[truncated]...\n";
    let room = max.saturating_sub(marker.chars().count());
    if room == 0 {
        return text.chars().take(max).collect();
    }
    let head_len = room * 2 / 3;
    let tail_len = room - head_len;
    let head: String = text.chars().take(head_len).collect();
    let tail: String = text.chars().skip(total - tail_len).collect();
    format!("{head}{marker}{tail}")
}

/// A styled span as an ANSI string, for the paths that have no screen. The
/// names are the terminal's (`Color::Cyan`), never an RGB value of ours, so the
/// user's theme decides what each one is. Redirected output is plain.
pub fn ansi(text: &str, style: Style) -> String {
    if !io::stdout().is_terminal() {
        return text.to_string();
    }
    let text = strip_control(text);
    let codes = codes(style);
    if codes.is_empty() {
        text
    } else {
        format!("\x1b[{}m{text}\x1b[0m", codes.join(";"))
    }
}

fn codes(style: Style) -> Vec<&'static str> {
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
