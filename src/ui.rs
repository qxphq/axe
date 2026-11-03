//! The screen: a fixed live region at the bottom of the terminal, the
//! transcript above it.
//!
//! `ratatui` owns everything that used to be arithmetic here: how many rows the
//! prompt occupies, where the cursor goes, what to repaint after a resize, and
//! how to push a line into scrollback without losing the drawing. This module
//! only decides *what* the live region contains: a blank row that keeps the
//! transcript off the prompt, the queued row, the prompt line, and one row
//! under it with the status on the left and the selection on the right.

use std::io::{self, IsTerminal, Stdout, Write};

use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::crossterm::{cursor, execute, terminal};
use ratatui::layout::{Alignment, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use ratatui::{Terminal, TerminalOptions, Viewport};
use unicode_width::UnicodeWidthChar;

/// Rows the live region always occupies: a blank gap, the queue, the prompt,
/// and the status and selection row. Fixed, so the viewport never moves under
/// the cursor, and the transcript never reaches the prompt.
const LIVE: u16 = 4;

/// Blank cells at each screen edge: nothing is written against the border.
const PAD: u16 = 1;

const PROMPT_STYLE: Style = Style::new().fg(Color::Cyan);
const DIM: Style = Style::new().add_modifier(Modifier::DIM);
const ERROR_STYLE: Style = Style::new().fg(Color::Red);

/// The kind of the last transcript block. Everything but two tool calls in a
/// row is separated from what came before it by one blank line.
#[derive(Clone, Copy, PartialEq)]
pub enum Block {
    /// The line the user committed.
    User,
    /// Reasoning.
    Thought,
    /// Model text, or a command's output.
    Text,
    /// A tool call and its outcome.
    Tool,
}

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
    /// Left half of the row under the prompt: thinking, or what is running.
    status: String,
    /// The line submitted mid-turn, waiting for the boundary. Its own row,
    /// above the prompt: the queue grows where the user is about to type, and
    /// a queued line that scrolls away silently looks like the Enter was lost.
    queued: Option<String>,
    /// Right half of the row under the prompt, permanent: what this session is
    /// pointed at. Set by the caller, which owns the model and the thinking
    /// level.
    info: String,
    /// The last killed text: one slot, enough for Ctrl-U ... Ctrl-Y.
    killed: String,
    /// Slash commands Tab completes, in the caller's spelling.
    commands: Vec<&'static str>,
    /// The last block written, for the blank line between blocks.
    last: Option<Block>,
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
            info: String::new(),
            killed: String::new(),
            commands: Vec::new(),
            last: None,
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

    pub fn set_info(&mut self, text: &str) {
        self.info = text.to_string();
    }

    /// The slash commands Tab completes.
    pub fn set_commands(&mut self, commands: &[&'static str]) {
        self.commands = commands.to_vec();
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
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Enter => return Action::Submit,
            KeyCode::Backspace if alt || ctrl => self.kill_word_back(),
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete => self.delete(),
            KeyCode::Left if alt || ctrl => self.word_back(),
            KeyCode::Right if alt || ctrl => self.word_forward(),
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.len()),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.len(),
            KeyCode::Tab => self.complete_command(),
            // A shell's Ctrl-C: drop what was typed, keep the prompt.
            KeyCode::Char('c') if ctrl => {
                self.buf.clear();
                self.cursor = 0;
            }
            KeyCode::Char('d') if ctrl && self.buf.is_empty() => return Action::Eof,
            KeyCode::Char('d') if ctrl => self.delete(),
            KeyCode::Char('a') if ctrl => self.cursor = 0,
            KeyCode::Char('e') if ctrl => self.cursor = self.len(),
            KeyCode::Char('b') if ctrl => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Char('f') if ctrl => self.cursor = (self.cursor + 1).min(self.len()),
            KeyCode::Char('b') if alt => self.word_back(),
            KeyCode::Char('f') if alt => self.word_forward(),
            KeyCode::Char('w') if ctrl || alt => self.kill_word_back(),
            KeyCode::Char('d') if alt => self.kill_word_forward(),
            KeyCode::Char('u') if ctrl => self.kill_to_start(),
            KeyCode::Char('k') if ctrl => self.kill_to_end(),
            KeyCode::Char('y') if ctrl => self.yank(),
            KeyCode::Char('t') if ctrl => self.transpose_chars(),
            KeyCode::Char('t') if alt => self.transpose_words(),
            KeyCode::Char(c) if !ctrl && !alt => self.insert_char(c),
            _ => {}
        }
        Action::None
    }

    /// Commit the line: echo it into the transcript and return it.
    pub fn commit(&mut self) -> String {
        let line = self.take();
        let text = format!("{}{}", self.prompt, line);
        self.insert(vec![Line::styled(text, PROMPT_STYLE)], Block::User);
        self.status.clear();
        line
    }

    /// Append lines to the transcript, above the live region. A line longer
    /// than the screen is wrapped, which is what the terminal would have done:
    /// the transcript scrolls, so it must not lose the tail of a long line.
    ///
    /// Blocks are separated by one blank line, so a message never sits against
    /// the tool call that followed it. Consecutive tool calls are one block:
    /// they belong together. So are the fragments of a message or a thought,
    /// which arrive split at their newlines: a separator per fragment would
    /// put a blank line between the lines of one paragraph.
    pub fn insert(&mut self, lines: Vec<Line<'static>>, block: Block) {
        let mut lines = lines;
        // A run of one kind is one block, except user lines: each is its own.
        let continues = block != Block::User && self.last == Some(block);
        if !continues {
            lines.insert(0, Line::default());
        }
        self.last = Some(block);
        let width = self.term.size().map(|s| s.width).unwrap_or(80).max(1) as usize;
        // Wrap and render inside the margin, so nothing lands on the edge.
        let margin = PAD.min(width.saturating_sub(1) as u16);
        let width = width.saturating_sub(2 * margin as usize).max(1);
        let rows: Vec<Line<'static>> = lines.into_iter().flat_map(|line| wrap(line, width)).collect();
        let height = rows.len() as u16;
        let _ = self.term.insert_before(height, |buf| {
            Paragraph::new(rows).render(inset(buf.area, margin), buf);
        });
        let _ = self.draw();
    }

    /// Plain text, split into transcript lines. A blank line at the start is
    /// the separator the sink wrote for its own output, and `insert` owns that
    /// here; blank lines inside the text are paragraph breaks and stay.
    pub fn text(&mut self, text: &str) {
        let mut lines: Vec<&str> = split(text);
        if self.last != Some(Block::Text) {
            let lead = lines.iter().take_while(|line| line.is_empty()).count();
            lines.drain(..lead);
        }
        let lines: Vec<Line<'static>> = lines
            .into_iter()
            .map(|line| Line::raw(line.to_string()))
            .collect();
        if !lines.is_empty() {
            self.insert(lines, Block::Text);
        }
    }

    /// Reasoning, dim: the model's notes, not its answer. Blank text is a
    /// no-op, so a round without reasoning adds no gap.
    pub fn thought(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let lines: Vec<Line<'static>> = split(text)
            .into_iter()
            .map(|line| Line::styled(line.to_string(), DIM))
            .collect();
        self.insert(lines, Block::Thought);
    }

    /// A warning or an error: same as `text`, in red.
    pub fn error(&mut self, text: &str) {
        let lines: Vec<Line<'static>> = split(text)
            .into_iter()
            .map(|line| Line::styled(line.to_string(), ERROR_STYLE))
            .collect();
        if !lines.is_empty() {
            self.insert(lines, Block::Text);
        }
    }

    /// Repaint the live region.
    pub fn draw(&mut self) -> io::Result<()> {
        let prompt = self.prompt.clone();
        let width = self.term.size()?.width;
        let margin = PAD.min(width.saturating_sub(1));
        let cols = (width as usize).saturating_sub(2 * margin as usize);
        let avail = cols.saturating_sub(prompt.chars().count()).max(1);
        // One row, scrolled horizontally: no wrapping to predict.
        let scroll = if self.cursor >= avail { self.cursor + 1 - avail } else { 0 };
        let visible: String = self.buf.chars().skip(scroll).take(avail).collect();
        let col = (prompt.chars().count() + self.cursor - scroll) as u16;
        let queued = self.queued.clone();
        // The selection is the invariant half of the last row: it must not be
        // cut, so a long status is trimmed first. One blank cell keeps the two
        // apart.
        let info = clip(&self.info, cols);
        let status = clip(
            &self.status,
            cols.saturating_sub(info.chars().count() + 1),
        );

        self.term.draw(|frame| {
            let area = frame.area();
            let area = inset(area, margin);
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
                row(2),
            );
            if area.height > 3 {
                frame.render_widget(
                    Paragraph::new(Line::styled(status.clone(), DIM)),
                    row(3),
                );
                frame.render_widget(
                    Paragraph::new(Line::styled(info.clone(), DIM))
                        .alignment(Alignment::Right),
                    row(3),
                );
            }
            if area.height > 1 {
                let label = match &queued {
                    Some(line) => clip(&format!("queued: {line}"), cols),
                    None => String::new(),
                };
                frame.render_widget(Paragraph::new(Line::styled(label, DIM)), row(1));
            }
            frame.set_cursor_position(Position::new(area.x + col, area.y + 2));
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

    /// Tab on a bare slash word: extend it to the longest common prefix, and
    /// finish it with a space when exactly one command matches. More than one
    /// match also lists them, so a shared prefix like `/th` never leaves the
    /// user guessing what was completed to. Arguments are not the screen's
    /// business, so a line that already has one is left alone.
    fn complete_command(&mut self) {
        if !self.buf.starts_with('/') || self.buf.contains(char::is_whitespace) {
            return;
        }
        let matches: Vec<&'static str> = self
            .commands
            .iter()
            .copied()
            .filter(|c| c.starts_with(&self.buf))
            .collect();
        let Some(first) = matches.first() else {
            return;
        };
        let mut prefix = first.to_string();
        for command in &matches[1..] {
            let common = prefix
                .bytes()
                .zip(command.bytes())
                .take_while(|(a, b)| a == b)
                .count();
            prefix.truncate(common);
        }
        if matches.len() == 1 {
            prefix.push(' ');
        }
        self.buf = prefix;
        self.cursor = self.len();
        if matches.len() > 1 {
            self.insert(vec![Line::styled(matches.join("  "), DIM)], Block::Text);
        }
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

    /// Replace chars `from..to` with `text`, leaving the cursor after it.
    fn splice(&mut self, from: usize, to: usize, text: &str) {
        let start = self.offset(from);
        let end = self.offset(to);
        self.buf.replace_range(start..end, text);
        self.cursor = from + text.chars().count();
    }

    /// `nth` from the end, where 0 is the last char. Cuts on char boundaries.
    fn at(&self, nth: usize) -> usize {
        self.offset(self.len().saturating_sub(nth))
    }

    fn char_at(&self, i: usize) -> Option<char> {
        self.buf.chars().nth(i)
    }

    /// Start of the word ending at or before the cursor: skip the whitespace
    /// the cursor sits in (or just after), then the word before it.
    fn word_start(&self) -> usize {
        let mut i = self.cursor;
        while i > 0 && self.char_at(i - 1).is_some_and(char::is_whitespace) {
            i -= 1;
        }
        while i > 0 && self.char_at(i - 1).is_some_and(|c| !c.is_whitespace()) {
            i -= 1;
        }
        i
    }

    /// End of the word starting at or after the cursor.
    fn word_end(&self) -> usize {
        let mut i = self.cursor;
        while i < self.len() && self.char_at(i).is_some_and(char::is_whitespace) {
            i += 1;
        }
        while i < self.len() && self.char_at(i).is_some_and(|c| !c.is_whitespace()) {
            i += 1;
        }
        i
    }

    fn word_back(&mut self) {
        self.cursor = self.word_start();
    }

    fn word_forward(&mut self) {
        self.cursor = self.word_end();
    }

    fn kill_word_back(&mut self) {
        let start = self.word_start();
        if start < self.cursor {
            self.killed = self.buf[self.offset(start)..self.offset(self.cursor)].to_string();
            self.splice(start, self.cursor, "");
        }
    }

    fn kill_word_forward(&mut self) {
        let end = self.word_end();
        if end > self.cursor {
            self.killed = self.buf[self.offset(self.cursor)..self.offset(end)].to_string();
            self.splice(self.cursor, end, "");
        }
    }

    fn kill_to_start(&mut self) {
        if self.cursor > 0 {
            self.killed = self.buf[..self.offset(self.cursor)].to_string();
            self.splice(0, self.cursor, "");
        }
    }

    fn kill_to_end(&mut self) {
        if self.cursor < self.len() {
            self.killed = self.buf[self.offset(self.cursor)..].to_string();
            self.splice(self.cursor, self.len(), "");
        }
    }

    fn yank(&mut self) {
        // Copied out first: `splice` takes `&mut self`.
        let text = self.killed.clone();
        if text.is_empty() {
            return;
        }
        let at = self.cursor;
        self.splice(at, at, &text);
    }

    /// Swap the two chars before the cursor, as readline does: at the end of
    /// the line the cursor steps back one first.
    fn transpose_chars(&mut self) {
        let end = if self.cursor == self.len() {
            self.at(0)
        } else {
            self.cursor
        };
        if end < 2 {
            return;
        }
        let (a, b) = (end - 2, end - 1);
        let (x, y) = (self.char_at(a).unwrap(), self.char_at(b).unwrap());
        self.splice(a, b + 1, &format!("{y}{x}"));
        // The cursor is a char index, so a multi-cell char is one step.
        self.cursor = a + 2;
    }

    /// Swap the two words before the cursor.
    fn transpose_words(&mut self) {
        // Step back over trailing whitespace, then find the word before it.
        let mut second_end = self.cursor;
        while second_end > 0 && self.char_at(second_end - 1).is_some_and(char::is_whitespace) {
            second_end -= 1;
        }
        let mut second_start = second_end;
        while second_start > 0 && self.char_at(second_start - 1).is_some_and(|c| !c.is_whitespace()) {
            second_start -= 1;
        }
        if second_start == second_end {
            return;
        }
        let mut first_end = second_start;
        while first_end > 0 && self.char_at(first_end - 1).is_some_and(char::is_whitespace) {
            first_end -= 1;
        }
        let mut first_start = first_end;
        while first_start > 0 && self.char_at(first_start - 1).is_some_and(|c| !c.is_whitespace()) {
            first_start -= 1;
        }
        if first_start == first_end {
            return;
        }
        let first = self.buf[self.offset(first_start)..self.offset(first_end)].to_string();
        let between = self.buf[self.offset(first_end)..self.offset(second_start)].to_string();
        let second = self.buf[self.offset(second_start)..self.offset(second_end)].to_string();
        self.splice(first_start, second_end, &format!("{second}{between}{first}"));
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        self.close();
    }
}

/// Fit text into `width` cells. The selection row is the only row whose length
/// the user controls (`model` ids and thinking levels), so a narrow terminal
/// gets an ellipsis instead of a word cut in half.
fn clip(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if text.chars().count() <= width {
        return text.to_string();
    }
    let mut out: String = text.chars().take(width.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// The area with `pad` blank cells taken off the left and right edges.
fn inset(area: Rect, pad: u16) -> Rect {
    Rect {
        x: area.x + pad,
        width: area.width.saturating_sub(2 * pad),
        ..area
    }
}

/// Break a styled line into display rows of at most `width` cells. Styles are
/// carried across the break, so a wrapped echo keeps its color.
fn wrap(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    let mut rows: Vec<Line<'static>> = Vec::new();
    let mut row: Vec<Span<'static>> = Vec::new();
    let mut col = 0usize;
    // A style set on the line does not reach its spans: `Line::styled` keeps it
    // on the line, and the rows built here are new, so patch it in per span.
    let base = line.style;
    for span in line.spans {
        let style = base.patch(span.style);
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
