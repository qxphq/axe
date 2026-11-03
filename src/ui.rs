//! The screen: a fixed live region at the bottom of the terminal, the
//! transcript above it.
//!
//! `ratatui` owns everything that used to be arithmetic here: how many rows the
//! prompt occupies, where the cursor goes, what to repaint after a resize, and
//! how to push a line into scrollback without losing the drawing. This module
//! only decides *what* the live region contains: the prompt line, a status row,
//! and the queued row.

use std::io::{self, IsTerminal, Stdout, Write};

use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::crossterm::{cursor, execute, terminal};
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use ratatui::{Terminal, TerminalOptions, Viewport};
use unicode_width::UnicodeWidthChar;

/// Rows the live region always occupies: prompt, status, queued. Fixed, so the
/// viewport never moves under the cursor.
const LIVE: u16 = 3;

const PROMPT_STYLE: Style = Style::new().fg(Color::Cyan);
const DIM: Style = Style::new().add_modifier(Modifier::DIM);
const ERROR_STYLE: Style = Style::new().fg(Color::Red);

/// What a key asked the caller to do.
pub enum Action {
    /// Nothing to submit.
    None,
    /// The line is complete; the caller takes it with `commit`.
    Submit,
    /// End of input.
    Eof,
}

pub struct Screen {
    term: Terminal<CrosstermBackend<Stdout>>,
    /// Plain text: the editor owns the color.
    prompt: String,
    buf: String,
    /// Cursor position in `buf`, in chars.
    cursor: usize,
    /// One dim line under the prompt: thinking, or the line being written.
    status: String,
    /// The line submitted mid-turn, waiting for the boundary. Its own row: the
    /// status row changes constantly, and a queued line that scrolls away
    /// silently looks like the Enter was lost.
    queued: Option<String>,
    restored: bool,
}

impl Screen {
    /// Take the terminal. Fails if there is no terminal to take, or if the
    /// terminal cannot be put into raw mode.
    pub fn new(prompt: &str) -> io::Result<Screen> {
        // No alternate screen: the transcript is the user's scrollback.
        let term = ratatui::try_init_with_options(TerminalOptions {
            viewport: Viewport::Inline(LIVE),
        })?;
        Ok(Screen {
            term,
            prompt: prompt.to_string(),
            buf: String::new(),
            cursor: 0,
            status: String::new(),
            queued: None,
            restored: false,
        })
    }

    pub fn prompt(&self) -> &str {
        &self.prompt
    }

    pub fn set_prompt(&mut self, prompt: &str) {
        self.prompt = prompt.to_string();
    }

    pub fn status(&self) -> &str {
        &self.status
    }

    pub fn set_status(&mut self, text: &str) {
        self.status = text.to_string();
    }

    pub fn set_queued(&mut self, line: Option<&str>) {
        self.queued = line.map(str::to_string);
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// The completed line, leaving the buffer empty.
    pub fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.buf)
    }

    /// Fold one key into the buffer. Holds no interrupt semantics: ESC belongs
    /// to the caller, which knows whether a turn is in flight.
    pub fn apply_key(&mut self, key: KeyEvent) -> Action {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Enter => return Action::Submit,
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete => self.delete(),
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.len()),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.len(),
            // A shell's Ctrl-C: drop what was typed, keep the prompt.
            KeyCode::Char('c') if ctrl => self.buf.clear(),
            KeyCode::Char('d') if ctrl && self.buf.is_empty() => return Action::Eof,
            KeyCode::Char(c) if !ctrl => self.insert_char(c),
            _ => {}
        }
        Action::None
    }

    /// Commit the line: echo it into the transcript and return it.
    pub fn commit(&mut self) -> String {
        let line = self.take();
        let text = format!("{}{}", self.prompt, line);
        self.insert(vec![Line::styled(text, PROMPT_STYLE)]);
        self.status.clear();
        line
    }

    /// Append lines to the transcript, above the live region. A line longer
    /// than the screen is wrapped, which is what the terminal would have done:
    /// the transcript scrolls, so it must not lose the tail of a long line.
    pub fn insert(&mut self, lines: Vec<Line<'static>>) {
        let width = self.term.size().map(|s| s.width).unwrap_or(80).max(1) as usize;
        let rows: Vec<Line<'static>> = lines.into_iter().flat_map(|line| wrap(line, width)).collect();
        let height = rows.len() as u16;
        let _ = self.term.insert_before(height, |buf| {
            Paragraph::new(rows).render(buf.area, buf);
        });
        let _ = self.draw();
    }

    /// Plain text, split into transcript lines. Blank lines are kept: they are
    /// the gap between blocks.
    pub fn text(&mut self, text: &str) {
        let lines: Vec<Line<'static>> = split(text)
            .into_iter()
            .map(|line| Line::raw(line.to_string()))
            .collect();
        if !lines.is_empty() {
            self.insert(lines);
        }
    }

    /// A warning or an error: same as `text`, in red.
    pub fn error(&mut self, text: &str) {
        let lines: Vec<Line<'static>> = split(text)
            .into_iter()
            .map(|line| Line::styled(line.to_string(), ERROR_STYLE))
            .collect();
        if !lines.is_empty() {
            self.insert(lines);
        }
    }

    /// Repaint the live region.
    pub fn draw(&mut self) -> io::Result<()> {
        let prompt = self.prompt.clone();
        let width = self.term.size()?.width;
        let avail = (width as usize).saturating_sub(prompt.chars().count()).max(1);
        // One row, scrolled horizontally: no wrapping to predict.
        let scroll = if self.cursor >= avail { self.cursor + 1 - avail } else { 0 };
        let visible: String = self.buf.chars().skip(scroll).take(avail).collect();
        let col = (prompt.chars().count() + self.cursor - scroll) as u16;
        let status = self.status.clone();
        let queued = self.queued.clone();

        self.term.draw(|frame| {
            let area = frame.area();
            let row = |offset: u16| Rect {
                y: area.y + offset,
                height: 1,
                ..area
            };
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(prompt.clone(), PROMPT_STYLE),
                    Span::raw(visible.clone()),
                ])),
                row(0),
            );
            if area.height > 1 {
                frame.render_widget(Paragraph::new(Line::styled(status.clone(), DIM)), row(1));
            }
            if area.height > 2 {
                let label = match &queued {
                    Some(line) => format!("queued: {line}"),
                    None => String::new(),
                };
                frame.render_widget(Paragraph::new(Line::styled(label, DIM)), row(2));
            }
            frame.set_cursor_position(Position::new(area.x + col, area.y));
        })
        .map(|_| ())
    }

    /// Give the terminal back: erase the live region so the shell prompt lands
    /// where it was, and leave raw mode.
    pub fn close(&mut self) {
        if self.restored {
            return;
        }
        self.restored = true;
        let area = self.term.get_frame().area();
        let _ = execute!(self.term.backend_mut(), cursor::MoveTo(0, area.y), terminal::Clear(terminal::ClearType::FromCursorDown), cursor::Show);
        let _ = self.term.flush();
        ratatui::restore();
    }

    fn len(&self) -> usize {
        self.buf.chars().count()
    }

    /// Byte offset of a char index.
    fn offset(&self, chars: usize) -> usize {
        self.buf
            .char_indices()
            .nth(chars)
            .map(|(byte, _)| byte)
            .unwrap_or(self.buf.len())
    }

    fn insert_char(&mut self, c: char) {
        let at = self.offset(self.cursor);
        self.buf.insert(at, c);
        self.cursor += 1;
    }

    fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            let at = self.offset(self.cursor);
            self.buf.remove(at);
        }
    }

    fn delete(&mut self) {
        if self.cursor < self.len() {
            let at = self.offset(self.cursor);
            self.buf.remove(at);
        }
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        self.close();
    }
}

/// Break a styled line into display rows of at most `width` cells. Styles are
/// carried across the break, so a wrapped echo keeps its color.
fn wrap(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    let mut rows: Vec<Line<'static>> = Vec::new();
    let mut row: Vec<Span<'static>> = Vec::new();
    let mut col = 0usize;
    for span in line.spans {
        let style = span.style;
        let mut text = String::new();
        for ch in span.content.chars() {
            let cells = ch.width().unwrap_or(1);
            if col > 0 && col + cells > width {
                if !text.is_empty() {
                    row.push(Span::styled(std::mem::take(&mut text), style));
                }
                rows.push(Line::from(std::mem::take(&mut row)));
                col = 0;
            }
            text.push(ch);
            col += cells;
        }
        if !text.is_empty() {
            row.push(Span::styled(text, style));
        }
    }
    rows.push(Line::from(row));
    rows
}

/// Text to transcript lines. A newline ends a line, so text that starts with
/// one starts with a blank line — which is exactly what a separator is. The
/// last one only closes the final line, so it must not add an empty one.
fn split(text: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = text.split('\n').collect();
    if text.ends_with('\n') {
        lines.pop();
    }
    lines
}

/// Read one line with cooked input. Used when stdin or stdout is not a tty:
/// there is no terminal to own, so the kernel does the editing.
pub fn cooked_line(prompt: &str) -> Option<String> {
    print!("{prompt}");
    io::stdout().flush().ok();
    let mut line = String::new();
    if io::BufRead::read_line(&mut io::stdin().lock(), &mut line).ok()? == 0 {
        return None;
    }
    // With a tty the kernel echoed the line, Enter included, so the prompt line
    // is already terminated. Without one nothing was echoed: close it here.
    if !io::stdin().is_terminal() {
        println!();
    }
    Some(line.trim_end_matches(['\n', '\r']).to_string())
}

/// True when the screen can own the terminal.
pub fn is_interactive() -> bool {
    io::stdin().is_terminal() && io::stdout().is_terminal()
}
