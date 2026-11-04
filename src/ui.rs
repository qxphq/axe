//! The screen: a fixed live region at the bottom of the terminal, the
//! transcript above it.
//!
//! `ratatui` owns everything that used to be arithmetic here: how many rows the
//! prompt occupies, where the cursor goes, what to repaint after a resize, and
//! how to push a line into scrollback without losing the drawing. This module
//! only decides *what* the live region contains: a blank row that keeps the
//! transcript off the prompt, the queued row, one prompt row per wrapped line,
//! an empty row under them, and below that one row with the status on the left
//! and the selection on the right.

use std::io::{self, IsTerminal, Stdout, Write};

use ratatui::backend::{Backend, ClearType, CrosstermBackend, WindowSize};
use ratatui::buffer::Cell;
use ratatui::crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, KeyCode, KeyEvent, KeyModifiers,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::{cursor, execute, terminal};
use ratatui::layout::{Alignment, Position, Rect, Size};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Widget};
use ratatui::{Terminal, TerminalOptions, Viewport};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::md;

/// Rows the live region occupies besides the prompt itself: a blank gap, the
/// queue, an empty row under the prompt, the status and selection row, and a
/// blank row under it. Nothing is written against the bottom edge.
const CHROME: u16 = 5;

/// Blank cells at each screen edge: nothing is written against the border.
const PAD: u16 = 1;

const PROMPT_STYLE: Style = Style::new().fg(Color::Cyan);
/// Explore mode's prompt: the glyph says the tool set is read-only.
const EXPLORE_STYLE: Style = Style::new().fg(Color::Rgb(255, 165, 0));
/// Marker for an already committed line in the transcript.
const MARK: &str = "█";
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
    /// Shift+Tab: the caller swaps modes. Nothing is submitted.
    Explore,
    /// End of input.
    Eof,
}

pub struct Screen {
    term: Terminal<Tail<Stdout>>,
    /// Plain text: the editor owns the color.
    prompt: String,
    /// Color of the prompt glyph, switched by explore mode.
    prompt_style: Style,
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
    /// Markdown being collected. A block is laid out once it ends, not once
    /// per line: a table needs its whole header, a paragraph its last line.
    md: md::Stream,
    restored: bool,
}

impl Screen {
    /// Take the terminal. Fails if there is no terminal to take, or if the
    /// terminal cannot be put into raw mode.
    pub fn new(prompt: &str) -> io::Result<Screen> {
        // No alternate screen: the transcript is the user's scrollback.
        // `try_init_with_options` hard-codes its backend, so what it does is
        // done here: restore the terminal if this panics, then raw mode.
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            ratatui::restore();
            hook(info);
        }));
        terminal::enable_raw_mode()?;
        let term = Terminal::with_options(
            Tail(CrosstermBackend::new(io::stdout())),
            TerminalOptions {
                viewport: Viewport::Inline(CHROME + 1),
            },
        )?;
        // Shift+Enter is only distinguishable from Enter under the kitty
        // keyboard protocol; bracketed paste keeps a pasted block one buffer
        // instead of one submission per line. A terminal that knows neither
        // ignores both.
        let _ = execute!(
            io::stdout(),
            EnableBracketedPaste,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES),
        );
        Ok(Screen {
            term,
            prompt: prompt.to_string(),
            prompt_style: PROMPT_STYLE,
            buf: String::new(),
            cursor: 0,
            status: String::new(),
            queued: None,
            info: String::new(),
            killed: String::new(),
            commands: Vec::new(),
            last: None,
            md: md::Stream::new(),
            restored: false,
        })
    }

    pub fn prompt(&self) -> &str {
        &self.prompt
    }

    pub fn set_prompt(&mut self, prompt: &str) {
        self.prompt = prompt.to_string();
    }

    /// Explore mode colors the prompt; the transcript keeps its own palette.
    pub fn set_explore(&mut self, explore: bool) {
        self.prompt_style = if explore { EXPLORE_STYLE } else { PROMPT_STYLE };
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
            // A new line, not a submission. Shift needs the kitty protocol;
            // Alt is the ESC-prefixed form every terminal sends.
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT)
                || key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.insert_char('\n')
            }
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
            KeyCode::BackTab => return Action::Explore,
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

    /// Commit the line: echo it into the transcript and return it. The marker
    /// is `█`, not the live prompt's `❯`: the line is settled, not editable.
    /// Every row carries it, so a wrapped line reads as one block.
    /// What was typed is markdown, laid out here because it is complete.
    pub fn commit(&mut self) -> String {
        let line = self.take();
        let mut lines = md::document(&line, self.cols().saturating_sub(2));
        if lines.is_empty() {
            lines.push(Line::default());
        }
        let rows: Vec<Line<'static>> = lines
            .into_iter()
            .map(|row| {
                let mut spans = vec![Span::styled(format!("{MARK} "), PROMPT_STYLE)];
                spans.extend(row.spans);
                Line::from(spans)
            })
            .collect();
        self.write(rows, Block::User);
        self.status.clear();
        line
    }

    /// Append lines to the transcript, above the live region. A line longer
    /// than the screen is wrapped, which is what the terminal would have done:
    /// the transcript scrolls, so it must not lose the tail of a long line.
    ///
    /// Blocks are separated by one blank line, so a message never sits against
    /// the tool call that followed it. Consecutive tool calls are one block:
    /// they belong together, and so are the lines of one thought.
    pub fn insert(&mut self, lines: Vec<Line<'static>>, block: Block) {
        // Anything that is not model text ends the block being collected: a
        // paragraph must not straddle a tool call, a thought, or a turn.
        if block != Block::Text {
            self.flush();
        }
        self.write(lines, block);
    }

    /// End the collected markdown: what it rendered to joins the transcript.
    pub fn flush(&mut self) {
        let mut lines = Vec::new();
        self.md.flush(self.cols(), &mut lines);
        if !lines.is_empty() {
            self.write(lines, Block::Text);
        }
    }

    /// The width inside the margins.
    fn cols(&self) -> usize {
        let width = self.term.size().map(|s| s.width).unwrap_or(80).max(1) as usize;
        let margin = PAD.min(width.saturating_sub(1) as u16) as usize;
        width.saturating_sub(2 * margin).max(1)
    }

    fn write(&mut self, lines: Vec<Line<'static>>, block: Block) {
        let mut lines = lines;
        // A run of one kind is one block, except user lines, which are each
        // their own, and markdown, which arrives one laid-out block at a time.
        let continues =
            matches!(block, Block::Tool | Block::Thought) && self.last == Some(block);
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
            // The backend prints every cell it is handed, blank ones included,
            // so a row padded to the screen width would leave trailing spaces
            // in scrollback for a later, narrower terminal to reflow into rows
            // of white space. An empty symbol instead marks where the row's
            // text ends: `Tail` draws up to it and erases the rest, which also
            // wipes whatever the row held before.
            let width = buf.area.width.max(1) as usize;
            for row in buf.content.chunks_mut(width) {
                // Past the last cell that is not blank, and past the columns a
                // wide glyph covers: the mark goes after the character, never
                // inside it.
                let mut end = 0;
                for (at, cell) in row.iter().enumerate() {
                    if cell.symbol() != " " {
                        end = at + cell.symbol().width().max(1);
                    }
                }
                let end = end.min(row.len());
                for cell in &mut row[end..] {
                    cell.set_symbol("");
                }
            }
        });
        let _ = self.draw();
    }

    /// Model text, a line at a time. The lines are markdown: they are
    /// collected until the block they belong to ends, which is where they
    /// reach the transcript.
    pub fn text(&mut self, text: &str) {
        let width = self.cols();
        for line in split(text) {
            let mut lines = Vec::new();
            self.md.push(line, width, &mut lines);
            // One write per rendered block: the block separator belongs
            // between them, not once per chunk.
            if !lines.is_empty() {
                self.write(lines, Block::Text);
            }
        }
    }

    /// Text this binary produced, not the model's: written as it is, one
    /// transcript line per line. Markdown belongs to the model and the user.
    pub fn output(&mut self, text: &str) {
        self.flush();
        let lines: Vec<Line<'static>> = split(text)
            .into_iter()
            .map(|line| Line::raw(line.to_string()))
            .collect();
        if !lines.is_empty() {
            self.write(lines, Block::Text);
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

    /// A warning or an error: same as `output`, in red.
    pub fn error(&mut self, text: &str) {
        self.flush();
        let lines: Vec<Line<'static>> = split(text)
            .into_iter()
            .map(|line| Line::styled(line.to_string(), ERROR_STYLE))
            .collect();
        if !lines.is_empty() {
            self.write(lines, Block::Text);
        }
    }

    /// A failed tool call's own words, red, directly under the call: it is part
    /// of the call's block, so no blank line separates the two.
    pub fn fail(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let lines: Vec<Line<'static>> = split(text)
            .into_iter()
            .map(|line| Line::styled(format!("    {line}"), ERROR_STYLE))
            .collect();
        self.insert(lines, Block::Tool);
    }

    /// Repaint the live region.
    pub fn draw(&mut self) -> io::Result<()> {
        let prompt = self.prompt.clone();
        let prompt_style = self.prompt_style;
        let size = self.term.size()?;
        // A resize since the last frame changes the area under us; settle it
        // before deciding the height. A failure only delays it to the next draw.
        let _ = self.term.autoresize();
        let width = size.width;
        let margin = PAD.min(width.saturating_sub(1));
        let cols = (width as usize).saturating_sub(2 * margin as usize);
        let gutter: usize = prompt.chars().map(|c| c.width().unwrap_or(1)).sum();
        let max_rows = rows_max(size.height);
        let (rows, first, (crow, ccol)) =
            prompt_layout(gutter, &self.buf, self.cursor, cols, max_rows);
        let visible = &rows[first..];
        let rows_n = visible.len() as u16;
        self.fit(CHROME + rows_n)?;
        // Every row carries the glyph, so a wrapped line reads as one prompt.
        let prompt_rows: Text<'static> = visible
            .iter()
            .map(|row| {
                Line::from(vec![
                    Span::styled(prompt.clone(), prompt_style),
                    Span::raw(row.clone()),
                ])
            })
            .collect();
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
            let area = inset(frame.area(), margin);
            let row = |offset: u16| Rect {
                y: area.y + offset,
                height: 1,
                ..area
            };
            let below = 2 + rows_n + 1;
            frame.render_widget(
                Paragraph::new(prompt_rows),
                Rect {
                    y: area.y + 2,
                    height: rows_n,
                    ..area
                },
            );
            frame.render_widget(
                Paragraph::new(Line::styled(status, DIM)),
                row(below),
            );
            frame.render_widget(
                Paragraph::new(Line::styled(info, DIM))
                    .alignment(Alignment::Right),
                row(below),
            );
            let label = match &queued {
                // One row: a queued message's own line breaks would be lost.
                Some(line) => clip(&format!("Queued: {}", line.replace('\n', " ")), cols),
                None => String::new(),
            };
            frame.render_widget(Paragraph::new(Line::styled(label, DIM)), row(1));
            frame.set_cursor_position(Position::new(
                area.x + gutter as u16 + ccol,
                area.y + 2 + crow,
            ));
        })
        .map(|_| ())
    }

    /// Give the live region `height` rows. It keeps its bottom at the screen
    /// bottom, so growing it upward would cover the last transcript rows: they
    /// are pushed into scrollback first, which leaves blank rows for the new
    /// prompt lines.
    fn fit(&mut self, height: u16) -> io::Result<()> {
        let size = self.term.size()?;
        let height = height.min(size.height).max(1);
        let area = self.term.get_frame().area();
        if area.height == height {
            return Ok(());
        }
        if height > area.height {
            self.term.insert_before(height - area.height, |_| {})?;
        } else {
            // The rows the region gives up are above the new top, where the new
            // terminal's clear cannot reach.
            self.term.clear()?;
        }
        // An inline area starts at the cursor row and this one's bottom is the
        // screen's, so the cursor goes where the new top has to be.
        let top = size.height - height;
        self.term.set_cursor_position(Position::new(0, top))?;
        // That derivation asks the terminal where its cursor is, and crossterm
        // answers only while it holds the event reader: the input thread takes
        // it in short turns, so this can miss a few times before it lands.
        let mut tries = 0;
        let term = loop {
            match Terminal::with_options(
                Tail(CrosstermBackend::new(io::stdout())),
                TerminalOptions {
                    viewport: Viewport::Inline(height),
                },
            ) {
                Ok(term) => break term,
                Err(_) if tries < 50 => {
                    tries += 1;
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
                Err(e) => return Err(e),
            }
        };
        self.term = term;
        // Blank the whole region and force a full repaint.
        self.term.clear()?;
        Ok(())
    }

    /// Give the terminal back: erase the live region so the shell prompt lands
    /// where it was, and leave raw mode.
    pub fn close(&mut self) {
        if self.restored {
            return;
        }
        self.restored = true;
        let area = self.term.get_frame().area();
        let _ = execute!(self.term.backend_mut(), PopKeyboardEnhancementFlags, DisableBracketedPaste, cursor::MoveTo(0, area.y), terminal::Clear(terminal::ClearType::FromCursorDown), cursor::Show);
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

    /// Text from a paste, newlines and all: it joins the buffer as it is, so a
    /// pasted block is one submission, not one per line. A terminal may hand
    /// over CRLF; that is still one line break.
    pub fn insert_text(&mut self, text: &str) {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        if text.is_empty() {
            return;
        }
        let at = self.offset(self.cursor);
        self.buf.insert_str(at, &text);
        self.cursor += text.chars().count();
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

/// Prompt rows the live region may use: at most half the screen, so the
/// transcript keeps a place to be read, and never none.
fn rows_max(screen_height: u16) -> usize {
    let live = (screen_height / 2).max(CHROME + 1);
    live.saturating_sub(CHROME).max(1) as usize
}

/// Lay the prompt buffer out at `width` cells, with `gutter` cells at the left
/// of every row. Returns the rows (text after the gutter), the index of the
/// first visible row, and the cursor as (row in that window, column after the
/// gutter). A buffer that fills a row to its last cell puts the cursor on the
/// next, empty one: the text broke there, so the cursor did too.
fn prompt_layout(
    gutter: usize,
    buf: &str,
    cursor: usize,
    width: usize,
    rows_max: usize,
) -> (Vec<String>, usize, (u16, u16)) {
    let limit = width.saturating_sub(gutter).max(1);
    let len = buf.chars().count();
    let mut rows: Vec<String> = vec![String::new()];
    let mut col = 0usize;
    let mut at = (0usize, 0usize);
    for (i, ch) in buf.chars().enumerate() {
        if i == cursor {
            at = (rows.len() - 1, col);
        }
        // A newline is a row break the user chose: the rest of the line goes
        // on the next row, which starts under the same glyph.
        if ch == '\n' {
            rows.push(String::new());
            col = 0;
            continue;
        }
        let cells = ch.width().unwrap_or(1);
        if col > 0 && col + cells > limit {
            rows.push(String::new());
            col = 0;
        }
        rows.last_mut().expect("a row").push(ch);
        col += cells;
    }
    if cursor >= len {
        at = (rows.len() - 1, col);
        if col >= limit {
            rows.push(String::new());
            at = (rows.len() - 1, 0);
        }
    }
    let first = (at.0 + 1).saturating_sub(rows_max.max(1));
    (rows, first, ((at.0 - first) as u16, at.1 as u16))
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
pub(crate) fn split(text: &str) -> Vec<&str> {
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

/// The terminal, with one rule added: a cell whose symbol is empty ends its
/// row, and the rest of that row is erased instead of being filled with spaces.
///
/// Scrollback must not carry padding: a terminal narrowed later reflows every
/// printed cell, and a row of trailing spaces becomes a row of white space.
/// Erasing rather than writing also wipes what the row held before, which
/// `Terminal::insert_before` counts on — it never clears the rows it draws
/// into, because its buffer is not sparse. Only `Screen::write` empties cells,
/// and only in the dense buffer of a transcript insert; the live region is
/// repainted by diff, where a row that ends early must keep its right half.
struct Tail<W: Write>(CrosstermBackend<W>);

impl<W: Write> Write for Tail<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Write::flush(&mut self.0)
    }
}

impl<W: Write> Backend for Tail<W> {
    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        let cells: Vec<(u16, u16, &Cell)> = content.collect();
        let mut at = 0;
        while at < cells.len() {
            // One row of the buffer: the whole of it for an insert, only the
            // cells that changed for a repaint.
            let y = cells[at].1;
            let rows = cells[at..].iter().take_while(|cell| cell.1 == y).count();
            let row = &cells[at..at + rows];
            at += rows;
            // The cell behind a wide glyph is not drawn: the backend counts
            // one column per cell, so printing it would move the rest of the
            // row one to the right of where the buffer says it is.
            let mut drawn = Vec::with_capacity(row.len());
            let mut covered = 0;
            let mut erase = None;
            for cell in row {
                if cell.0 < covered {
                    continue;
                }
                if cell.2.symbol().is_empty() {
                    erase = Some(cell.0);
                    break;
                }
                drawn.push(*cell);
                covered = cell.0 + cell.2.symbol().width().max(1) as u16;
            }
            self.0.draw(drawn.into_iter())?;
            if let Some(x) = erase {
                self.0.set_cursor_position(Position::new(x, y))?;
                self.0.clear_region(ClearType::UntilNewLine)?;
            }
        }
        Ok(())
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        self.0.hide_cursor()
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        self.0.show_cursor()
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        self.0.get_cursor_position()
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        self.0.set_cursor_position(position)
    }

    fn clear(&mut self) -> io::Result<()> {
        self.0.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        self.0.clear_region(clear_type)
    }

    fn append_lines(&mut self, n: u16) -> io::Result<()> {
        self.0.append_lines(n)
    }

    fn size(&self) -> io::Result<Size> {
        self.0.size()
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        self.0.window_size()
    }

    fn flush(&mut self) -> io::Result<()> {
        Backend::flush(&mut self.0)
    }
}
