//! Text the harness handles itself: bytes that came from somewhere else.
//!
//! A fetched page, a command's stderr and the model's own prose all reach the
//! terminal through here. Three things are the harness's, not the content's:
//! control bytes (a page can carry an escape sequence), the output cap (a
//! command can print a gigabyte) and the style (a `Style` becomes SGR in one
//! place, so the palette is one table).

use std::io::{self, IsTerminal};

use ratatui::style::{Color, Modifier, Style};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Drop control bytes, keeping the two that lay text out. Content is never
/// trusted to be text: a page that carries ESC would otherwise paint.
pub fn strip_control(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect()
}

/// Whether a character may be drawn: a control byte paints on the terminal,
/// and an invisible format mark (a bidi override, a tag character) reorders or
/// hides what it shows.
pub(crate) fn shows(c: char) -> bool {
    !c.is_control() && !is_invisible(c)
}

/// The characters that occupy no cell but can reorder or hide text. The web
/// sanitizer strips them from a page; a transcript row and a frame field must
/// not carry them either.
pub(crate) fn is_invisible(c: char) -> bool {
    matches!(
        c as u32,
        0x00AD
            | 0x180E
            | 0x200B..=0x200F
            | 0x202A..=0x202E
            | 0x2060..=0x2064
            | 0x2066..=0x2069
            | 0xFEFF
            | 0xE0000..=0xE007F
    )
}

/// Text a transcript row or a frame field may carry: `shows` per character,
/// with no exception for the newline a row cannot use.
pub fn displayable(text: &str) -> String {
    text.chars().filter(|c| shows(*c)).collect()
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

/// The suffix a count takes: `format!("{n} item{}", plural(n))`.
pub fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// Fit text into `width` cells, ending with an ellipsis when it did not fit.
/// The one cap for text the user controls — a model id, a path, a title — so a
/// narrow row loses its tail rather than a word's second half.
pub fn clip(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let text = displayable(text);
    if text.width() <= width {
        return text.to_string();
    }
    let limit = width.saturating_sub(1);
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let cells = c.width().unwrap_or(1);
        if used + cells > limit {
            break;
        }
        out.push(c);
        used += cells;
    }
    out.push('…');
    out
}

/// The first line of `text`, capped at `max` cells, marked when anything was
/// dropped: a command or a tool call is one transcript line, whatever its
/// argument held.
pub fn oneline(text: &str, max: usize) -> String {
    let line = text.lines().next().unwrap_or("");
    let cut = line.width() > max;
    let mut out = clip(line, max);
    if !cut && text.lines().count() > 1 {
        out.push('…');
    }
    out
}

/// Keep both ends of `text`, `head` chars from the front and `tail` from the
/// back, marked where it was cut: the error is at the end and the command at
/// the start.
pub fn head_tail(text: &str, head: usize, tail: usize) -> String {
    let count = text.chars().count();
    if count <= head + tail + 1 {
        return text.to_string();
    }
    let front: String = text.chars().take(head).collect();
    let back: String = text.chars().skip(count - tail).collect();
    format!("{front}…[cut]…{back}")
}

/// A styled span as an ANSI string, for the paths that have no screen. The
/// names are the terminal's (`Color::Cyan`), never an RGB value of ours, so the
/// user's theme decides what each one is. Redirected output is plain.
pub fn ansi(text: &str, style: Style) -> String {
    if io::stdout().is_terminal() {
        styled(text, style)
    } else {
        text.to_string()
    }
}

/// The codes for a span, whether or not there is a terminal to read. `ansi`
/// decides by stdout; this is what it does once that is known, so the styling
/// itself can be checked without borrowing the process's own output.
pub fn styled(text: &str, style: Style) -> String {
    let text = strip_control(text);
    let codes = codes(style);
    if codes.is_empty() {
        text
    } else {
        let codes: Vec<String> = codes.iter().map(u8::to_string).collect();
        format!("\x1b[{}m{text}\x1b[0m", codes.join(";"))
    }
}

/// The SGR number of one of the terminal's own colors, and nothing for a color
/// this harness must not use: the palette is the terminal's, so an `Rgb` or
/// `Indexed` value here would be a bug, and an unnamed one lands on the default.
fn color_code(color: Color) -> Option<u8> {
    Some(match color {
        Color::Black => 30,
        Color::Red => 31,
        Color::Green => 32,
        Color::Yellow => 33,
        Color::Blue => 34,
        Color::Magenta => 35,
        Color::Cyan => 36,
        Color::Gray => 37,
        Color::DarkGray => 90,
        Color::LightRed => 91,
        Color::LightGreen => 92,
        Color::LightYellow => 93,
        Color::LightBlue => 94,
        Color::LightMagenta => 95,
        Color::LightCyan => 96,
        Color::White => 97,
        Color::Reset | Color::Indexed(_) | Color::Rgb(_, _, _) => return None,
    })
}

fn codes(style: Style) -> Vec<u8> {
    let mut codes = Vec::new();
    // Every named color, foreground and background: one table per color, so a
    // piece cannot be colored on the screen and plain in a pipe.
    if let Some(code) = style.fg.and_then(color_code) {
        codes.push(code);
    }
    if let Some(code) = style.bg.and_then(color_code) {
        codes.push(code + 10);
    }
    for (bit, code) in [
        (Modifier::BOLD, 1),
        (Modifier::DIM, 2),
        (Modifier::ITALIC, 3),
        (Modifier::UNDERLINED, 4),
        (Modifier::CROSSED_OUT, 9),
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

    /// The one cap behind `bash`, `web_search` and MCP: both ends survive, the
    /// result never grows past `max`, and a cut can never split a character.
    #[test]
    fn truncate_keeps_both_ends_without_splitting_a_character() {
        let out = truncate("é".repeat(2_000), 100);
        assert_eq!(out.chars().count(), 100, "{out}");
        assert!(out.contains("...[truncated]..."), "{out}");
        assert!(out.starts_with("éé"), "{out}");
        assert!(out.ends_with("éé"), "{out}");
        // An exact fit is left alone; one character more is cut to the cap.
        assert_eq!(truncate("x".repeat(100), 100), "x".repeat(100));
        assert_eq!(truncate("x".repeat(101), 100).chars().count(), 100);
        // Smaller than its own marker: the head is all there is room for.
        assert_eq!(truncate("y".repeat(50), 10).chars().count(), 10);
    }

    /// ANSI output only exists for a terminal, so the codes are what a test can
    /// read.
    #[test]
    fn every_named_color_has_a_code() {
        // Magenta was missing when the table was a hand-picked subset, so the
        // thinking level was colored on the screen and plain in a pipe.
        for (color, want) in [
            (Color::Black, 30),
            (Color::Red, 31),
            (Color::Green, 32),
            (Color::Yellow, 33),
            (Color::Blue, 34),
            (Color::Magenta, 35),
            (Color::Cyan, 36),
            (Color::Gray, 37),
            (Color::DarkGray, 90),
            (Color::LightRed, 91),
            (Color::LightGreen, 92),
            (Color::LightYellow, 93),
            (Color::LightBlue, 94),
            (Color::LightMagenta, 95),
            (Color::LightCyan, 96),
            (Color::White, 97),
        ] {
            assert_eq!(codes(Style::new().fg(color)), vec![want], "{color:?}");
            assert_eq!(codes(Style::new().bg(color)), vec![want + 10], "{color:?}");
        }
        // A color the palette does not use draws as the default.
        assert!(codes(Style::new().fg(Color::Reset)).is_empty());
        assert!(codes(Style::new().fg(Color::Rgb(1, 2, 3))).is_empty());
        assert!(codes(Style::new().fg(Color::Indexed(4))).is_empty());
    }

    #[test]
    fn modifiers_join_the_color() {
        let style = Style::new()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
        assert_eq!(codes(style), vec![36, 1, 4]);
        assert!(codes(Style::new()).is_empty());
    }
}
