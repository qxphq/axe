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

#[cfg(test)]
use ratatui::backend::TestBackend;
use ratatui::backend::{Backend, ClearType, CrosstermBackend, WindowSize};
use ratatui::buffer::{Buffer, Cell};
use ratatui::crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, KeyCode, KeyEvent, KeyModifiers,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::{cursor, execute, terminal};
use ratatui::layout::{Alignment, Position, Rect, Size};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use ratatui::{Frame, Terminal, TerminalOptions, Viewport};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::markdown;

/// Rows the live region occupies besides the prompt itself: a blank gap, the
/// queue, an empty row under the prompt, the status and selection row, and a
/// blank row under it. Nothing is written against the bottom edge.
const CHROME: u16 = 5;

/// Blank cells at each screen edge: nothing is written against the border.
const PAD: u16 = 1;

/// Rows the candidate list under the prompt may take. The list is a hint, not
/// a chooser: it never covers the transcript for long.
const HINTS: usize = 5;

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
    /// A session chooser is open and the selection moved.
    MenuStep(isize),
    /// Enter on an empty line with a chooser open.
    MenuAccept,
    /// Esc with a chooser open.
    MenuCancel,
    /// End of input.
    Eof,
}

/// A chooser above the prompt: startup's recent sessions, and `/sessions`.
/// The rows are already formatted; only the marker and the style are added
/// here. When it closes it can leave its rows in the transcript, so the list
/// survives and no blank rows are left where it was.
struct Menu {
    header: String,
    rows: Vec<String>,
    selected: usize,
    scroll: usize,
    /// Rows the last draw had room for. Zero means the chooser is open but not
    /// visible, so its keys must not act.
    visible: usize,
    /// Typing starts a new session instead of searching: startup's behaviour.
    collapse_on_type: bool,
}

/// A Tab cycle in flight: the candidates for what was typed before the first
/// Tab, and which one the buffer holds now. The candidates are captured once,
/// so a second Tab walks all of them even though the completed buffer no
/// longer matches the others.
struct Cycle {
    matches: Vec<String>,
    at: usize,
}

enum Term {
    Real(Terminal<Tail<Stdout>>),
    #[cfg(test)]
    Test(Terminal<TestBackend>),
}

impl Term {
    fn size(&self) -> io::Result<Size> {
        match self {
            Term::Real(term) => term.size(),
            #[cfg(test)]
            Term::Test(term) => term.size(),
        }
    }

    fn autoresize(&mut self) -> io::Result<()> {
        match self {
            Term::Real(term) => term.autoresize(),
            #[cfg(test)]
            Term::Test(term) => term.autoresize(),
        }
    }

    fn draw<F: FnOnce(&mut Frame)>(&mut self, draw: F) -> io::Result<()> {
        match self {
            Term::Real(term) => term.draw(draw).map(|_| ()),
            #[cfg(test)]
            Term::Test(term) => term.draw(draw).map(|_| ()),
        }
    }

    fn insert_before<F: FnOnce(&mut Buffer)>(&mut self, height: u16, draw: F) -> io::Result<()> {
        match self {
            Term::Real(term) => term.insert_before(height, draw),
            #[cfg(test)]
            Term::Test(term) => term.insert_before(height, draw),
        }
    }

    fn clear(&mut self) -> io::Result<()> {
        match self {
            Term::Real(term) => term.clear(),
            #[cfg(test)]
            Term::Test(term) => term.clear(),
        }
    }

    fn area(&mut self) -> Rect {
        match self {
            Term::Real(term) => term.get_frame().area(),
            #[cfg(test)]
            Term::Test(term) => term.get_frame().area(),
        }
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        match self {
            Term::Real(term) => term.set_cursor_position(position),
            #[cfg(test)]
            Term::Test(term) => term.set_cursor_position(position),
        }
    }
}

pub struct Screen {
    term: Term,
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
    /// The lines Tab completes, in the caller's spelling: a slash command on
    /// its own, or one with its first argument filled in.
    commands: Vec<String>,
    /// The Tab cycle the buffer is part of, if any. Any edit ends it.
    cycle: Option<Cycle>,
    /// The last block written, for the blank line between blocks.
    last: Option<Block>,
    /// Markdown being collected. A block is laid out once it ends, not once
    /// per line: a table needs its whole header, a paragraph its last line.
    md: markdown::Stream,
    /// Reasoning, collected the same way. Its own stream: a thought's last
    /// paragraph must not run into the answer's first.
    thought: markdown::Stream,
    /// A chooser above the prompt, when one is open.
    menu: Option<Menu>,
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
        // A terminal shorter than the normal live region must still start.
        let rows = terminal::size()
            .map(|(_, height)| height)
            .unwrap_or(CHROME + 1)
            .clamp(1, CHROME + 1);
        let term = match Terminal::with_options(
            Tail(CrosstermBackend::new(io::stdout())),
            TerminalOptions {
                viewport: Viewport::Inline(rows),
            },
        ) {
            Ok(term) => term,
            Err(e) => {
                let _ = terminal::disable_raw_mode();
                return Err(e);
            }
        };
        // Only disambiguation is asked for. Ascertaining Shift+Enter would
        // mean reporting every key as an escape code, which takes shifted
        // characters away from the terminal and makes this side guess a
        // keyboard layout; a new line is typed as Alt+Enter or Ctrl+J instead.
        // Bracketed paste keeps a pasted block one buffer, and a terminal that
        // knows neither ignores both.
        let _ = execute!(
            io::stdout(),
            EnableBracketedPaste,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES),
        );
        Ok(Screen {
            term: Term::Real(term),
            prompt: prompt.to_string(),
            prompt_style: PROMPT_STYLE,
            buf: String::new(),
            cursor: 0,
            status: String::new(),
            queued: None,
            info: String::new(),
            killed: String::new(),
            commands: Vec::new(),
            cycle: None,
            last: None,
            md: markdown::Stream::new(),
            thought: markdown::Stream::new(),
            menu: None,
            restored: false,
        })
    }

    #[cfg(test)]
    fn test_screen(width: u16, height: u16) -> Screen {
        Screen {
            term: Term::Test(Terminal::new(TestBackend::new(width, height)).unwrap()),
            prompt: "❯ ".to_string(),
            prompt_style: PROMPT_STYLE,
            buf: String::new(),
            cursor: 0,
            status: String::new(),
            queued: None,
            info: String::new(),
            killed: String::new(),
            commands: Vec::new(),
            cycle: None,
            last: None,
            md: markdown::Stream::new(),
            thought: markdown::Stream::new(),
            menu: None,
            restored: false,
        }
    }

    #[cfg(test)]
    fn test_buffer(&self) -> &Buffer {
        match &self.term {
            Term::Test(term) => term.backend().buffer(),
            Term::Real(_) => unreachable!(),
        }
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

    /// The lines Tab completes: slash names, and names with their first
    /// argument.
    pub fn set_commands(&mut self, commands: Vec<String>) {
        self.commands = commands;
    }

    /// Open a chooser. Rows are plain text; the marker is drawn on the
    /// selected one.
    pub fn set_menu(&mut self, header: &str, rows: Vec<String>, collapse_on_type: bool) {
        if rows.is_empty() {
            self.menu = None;
            return;
        }
        self.menu = Some(Menu {
            header: header.to_string(),
            rows,
            selected: 0,
            scroll: 0,
            visible: 0,
            collapse_on_type,
        });
    }

    /// Close the chooser, optionally leaving its rows in the transcript. They
    /// are committed without the selection marker: the choice is over.
    pub fn clear_menu(&mut self, commit: bool) {
        let Some(menu) = self.menu.take() else { return };
        // Shrink to the prompt first, so committed rows land above a live
        // region of its normal height rather than over the chooser's rows.
        let _ = self.draw();
        if commit && !menu.rows.is_empty() {
            let lines: Vec<Line<'static>> = menu
                .rows
                .into_iter()
                .map(|row| Line::styled(row, DIM))
                .collect();
            self.write(lines, Block::Text);
        }
    }

    pub fn menu_selected(&self) -> usize {
        self.menu.as_ref().map(|menu| menu.selected).unwrap_or(0)
    }

    pub fn menu_move(&mut self, delta: isize) {
        let Some(menu) = &mut self.menu else { return };
        let last = menu.rows.len().saturating_sub(1);
        menu.selected = menu.selected.saturating_add_signed(delta).min(last);
    }

    /// A chooser is active only when the last draw had room for it. An open
    /// menu on a short screen must not eat Enter or the arrows.
    fn menu_active(&self) -> bool {
        self.menu.as_ref().is_some_and(|menu| menu.visible > 0)
    }

    /// Visible entries: whatever the prompt does not need, but at least one.
    /// Zero when the screen is too short to hold a chooser at all: drawing one
    /// there would put the status row outside the terminal.
    fn menu_visible(&self, rows_n: u16, screen_h: u16) -> usize {
        let Some(menu) = &self.menu else { return 0 };
        let base = CHROME as usize + rows_n as usize;
        if (screen_h as usize) < base + 3 {
            return 0;
        }
        let room = (screen_h as usize).saturating_sub(base + 3).max(1);
        menu.rows.len().min(room)
    }

    /// Keep the selected row inside the visible window.
    fn menu_follow(&mut self, visible: usize) {
        let Some(menu) = &mut self.menu else { return };
        if visible == 0 {
            return;
        }
        if menu.selected < menu.scroll {
            menu.scroll = menu.selected;
        }
        if menu.selected >= menu.scroll + visible {
            menu.scroll = menu.selected + 1 - visible;
        }
        menu.scroll = menu.scroll.min(menu.rows.len().saturating_sub(visible));
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// The completed line, leaving the buffer empty.
    pub fn take(&mut self) -> String {
        self.cursor = 0;
        self.cycle = None;
        std::mem::take(&mut self.buf)
    }

    /// Fold one key into the buffer. Holds no interrupt semantics: ESC belongs
    /// to the caller, which knows whether a turn is in flight.
    pub fn apply_key(&mut self, key: KeyEvent) -> Action {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            // A chooser owns the arrows and Esc while it is open. Enter on an
            // empty line accepts it; Enter on a search query submits as usual.
            KeyCode::Up if self.menu_active() => return Action::MenuStep(-1),
            KeyCode::Down if self.menu_active() => return Action::MenuStep(1),
            KeyCode::Esc if self.menu.is_some() => return Action::MenuCancel,
            KeyCode::Enter if self.menu_active() && self.buf.is_empty() => {
                return Action::MenuAccept
            }
            // A new line, not a submission: Alt is the ESC-prefixed form every
            // terminal sends. Shift+Enter is deliberately not a binding: a
            // terminal reports it as a bare Enter unless the kitty protocol
            // says otherwise, and asking for that costs every shifted
            // character its layout.
            KeyCode::Enter if alt => self.insert_char('\n'),
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
            KeyCode::Tab => self.cycle_step(true),
            KeyCode::Up => self.cycle_step(false),
            KeyCode::Down => self.cycle_step(true),
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
            // Ctrl+J is the newline byte, and what a terminal-side Shift+Enter
            // mapping sends. One buffer character, not a submission.
            KeyCode::Char('j') if ctrl => self.insert_char('\n'),
            KeyCode::Char(c) if !ctrl && !alt => self.insert_char(c),
            _ => {}
        }
        // A cycle holds only while the buffer still is the candidate it put
        // there. Any edit ends it, so the next Tab starts from what is typed.
        if self
            .cycle
            .as_ref()
            .is_some_and(|c| c.matches.get(c.at) != Some(&self.buf))
        {
            self.cycle = None;
        }
        Action::None
    }

    /// Commit the line: echo it into the transcript and return it. The marker
    /// is `█`, not the live prompt's `❯`: the line is settled, not editable.
    /// Every row carries it, so a wrapped line reads as one block.
    /// What was typed is markdown, laid out here because it is complete.
    pub fn commit(&mut self) -> String {
        let line = self.take();
        self.echo(&line);
        self.status.clear();
        line
    }

    /// A user line from somewhere else: the resumed transcript. It is complete
    /// already, so the same layout as a committed line, without a buffer.
    pub fn echo_user(&mut self, line: &str) {
        self.flush();
        self.echo(line);
    }

    fn echo(&mut self, line: &str) {
        let mut lines = markdown::document(line, self.cols().saturating_sub(2));
        if lines.is_empty() {
            lines.push(Line::default());
        }
        let rows: Vec<Line<'static>> = lines
            .into_iter()
            .map(|row| {
                let mut spans = vec![Span::styled(format!("{MARK} "), PROMPT_STYLE)];
                spans.extend(row.spans.into_iter().map(|s| s.patch_style(PROMPT_STYLE)));
                Line::from(spans)
            })
            .collect();
        self.write(rows, Block::User);
    }

    /// Append lines to the transcript, above the live region. A line longer
    /// than the screen is wrapped, which is what the terminal would have done:
    /// the transcript scrolls, so it must not lose the tail of a long line.
    ///
    /// Blocks are separated by one blank line, so a message never sits against
    /// the tool call that followed it. Consecutive tool calls are one block:
    /// they belong together.
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
        // Reasoning may be the last thing a round wrote.
        self.end_thought();
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
        // Whatever follows a thought ends it, and the thought goes first.
        if block != Block::Thought {
            self.end_thought();
        }
        let mut lines = lines;
        // Consecutive tool calls are one block: they belong together. A user
        // line is its own, and markdown arrives one laid-out block at a time,
        // so neither continues.
        let continues = block == Block::Tool && self.last == Some(block);
        if !continues {
            lines.insert(0, Line::default());
        }
        self.last = Some(block);
        let width = self.term.size().map(|s| s.width).unwrap_or(80).max(1) as usize;
        // Wrap and render inside the margin, so nothing lands on the edge.
        let margin = PAD.min(width.saturating_sub(1) as u16);
        let width = width.saturating_sub(2 * margin as usize).max(1);
        let rows: Vec<Line<'static>> = lines
            .into_iter()
            .flat_map(|line| wrap(line, width))
            .collect();
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

    /// Reasoning, dim: the model's notes, not its answer. Markdown like the
    /// answer, and collected the same way: a block reaches the transcript when
    /// it ends.
    pub fn thought(&mut self, text: &str) {
        let width = self.cols();
        for line in split(text) {
            let mut lines = Vec::new();
            self.thought.push(line, width, &mut lines);
            self.notes(lines);
        }
    }

    /// End the reasoning: its last line was unfinished while it streamed, so
    /// the paragraph it belongs to is finished only now.
    fn end_thought(&mut self) {
        let mut lines = Vec::new();
        self.thought.flush(self.cols(), &mut lines);
        self.notes(lines);
    }

    /// Laid-out reasoning: the renderer's styles stay, dim goes on top, so
    /// bold, code and links are still told apart inside a thought.
    fn notes(&mut self, mut lines: Vec<Line<'static>>) {
        if lines.is_empty() {
            return;
        }
        for line in &mut lines {
            for span in &mut line.spans {
                span.style = span.style.add_modifier(Modifier::DIM);
            }
        }
        self.write(lines, Block::Thought);
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

    /// `/new`: the transcript above belongs to the session that just ended, so
    /// it goes with it, scrollback included, and the live region starts blank.
    /// The one place the transcript is erased: a session is a boundary.
    pub fn reset(&mut self) {
        // Collected markdown would otherwise join the new session's first
        // block; the transcript starts with no block to continue.
        self.md = markdown::Stream::new();
        self.thought = markdown::Stream::new();
        self.last = None;
        self.menu = None;
        self.status.clear();
        self.queued = None;
        #[cfg(test)]
        if matches!(self.term, Term::Test(_)) {
            return;
        }
        match &mut self.term {
            Term::Real(term) => {
                // `terminal::Clear(All)` only reaches the screen; what scrolled
                // off is in the terminal's own history, which `Purge` drops.
                let _ = execute!(
                    term.backend_mut(),
                    terminal::Clear(terminal::ClearType::All),
                    terminal::Clear(terminal::ClearType::Purge)
                );
                // Repaints the region once and, as importantly, resets the
                // back buffer, so the next draw is not diffed against a blank
                // screen.
                let _ = term.clear();
            }
            #[cfg(test)]
            Term::Test(_) => {}
        }
    }

    /// Repaint the live region.
    pub fn draw(&mut self) -> io::Result<()> {
        let prompt = self.prompt.clone();
        let prompt_style = self.prompt_style;
        // A resize since the last frame changes the area under us; settle it
        // before reading the size. A failure only delays it to the next draw.
        let _ = self.term.autoresize();
        let size = self.term.size()?;
        let width = size.width;
        let margin = PAD.min(width.saturating_sub(1));
        let cols = (width as usize).saturating_sub(2 * margin as usize);
        let gutter: usize = prompt.chars().map(|c| c.width().unwrap_or(1)).sum();
        let max_rows = rows_max(size.height);
        let (rows, first, (crow, ccol)) =
            prompt_layout(gutter, &self.buf, self.cursor, cols, max_rows);
        let visible = &rows[first..];
        let rows_n = visible.len() as u16;
        // The chooser takes what the prompt leaves, never the last row: the
        // transcript keeps a place to be read even with 15 sessions on screen.
        let slots = self.menu_visible(rows_n, size.height);
        self.menu_follow(slots);
        if let Some(menu) = &mut self.menu {
            menu.visible = slots;
        }
        let menu = self.menu.as_ref().filter(|_| slots > 0).map(|menu| {
            let start = menu.scroll;
            (
                menu.header.clone(),
                menu.rows[start..start + slots].to_vec(),
                menu.selected,
                menu.scroll,
            )
        });
        let menu_h = menu
            .as_ref()
            .map(|(_, entries, _, _)| entries.len() as u16 + 2)
            .unwrap_or(0);
        // The candidates go under the prompt, at most five of them, and the
        // window follows the selection so the one the next step takes is on
        // screen. The list lives inside the half screen the live region may
        // take, so a short terminal gets fewer rows, or none.
        let (hints, at) = self.hints();
        let live = (size.height / 2).max(CHROME + 1);
        let room = live.saturating_sub(CHROME + rows_n) as usize;
        let shown = hints.len().min(HINTS).min(room);
        let start = if shown == 0 {
            0
        } else {
            at.saturating_sub(shown - 1).min(hints.len() - shown)
        };
        let hints = &hints[start..];
        let hints = &hints[..hints.len().min(shown)];
        let hints_h = hints.len() as u16;
        self.fit(CHROME + rows_n + menu_h + hints_h)?;
        // Every row carries the glyph, so a wrapped line reads as one prompt.
        let prompt_rows: Vec<Line<'static>> = visible
            .iter()
            .map(|row| {
                Line::from(vec![
                    Span::styled(prompt.clone(), prompt_style),
                    Span::raw(row.clone()),
                ])
            })
            .collect();
        // Under the prompt's text, not under its glyph: a suggestion reads as
        // a continuation of what is being typed. The row the next step takes
        // is the bright one.
        let pad = " ".repeat(gutter.min(cols.saturating_sub(1)));
        let queued = self.queued.clone();
        // The selection is the invariant half of the last row: it must not be
        // cut, so a long status is trimmed first. One blank cell keeps the two
        // apart.
        let info = clip(&self.info, cols);
        let status = clip(&self.status, cols.saturating_sub(info.chars().count() + 1));

        self.term
            .draw(|frame| {
                let area = inset(frame.area(), margin);
                let row = |offset: u16| Rect {
                    y: area.y + offset,
                    height: 1,
                    ..area
                };
                let top = 2 + menu_h;
                let below = top + rows_n + hints_h + 1;
                if let Some((header, entries, selected, scroll)) = &menu {
                    frame.render_widget(
                        Paragraph::new(Line::styled(clip(header, cols), DIM)),
                        row(2),
                    );
                    for (at, entry) in entries.iter().enumerate() {
                        let on = *selected == scroll + at;
                        let marker = if on { "▸ " } else { "  " };
                        let text = clip(entry, cols.saturating_sub(2));
                        frame.render_widget(
                            Paragraph::new(Line::styled(
                                format!("{marker}{text}"),
                                if on { PROMPT_STYLE } else { DIM },
                            )),
                            row(3 + at as u16),
                        );
                    }
                }
                frame.render_widget(
                    Paragraph::new(prompt_rows),
                    Rect {
                        y: area.y + top,
                        height: rows_n,
                        ..area
                    },
                );
                for (i, hint) in hints.iter().enumerate() {
                    let on = start + i == at;
                    let text = clip(hint, cols.saturating_sub(pad.len()));
                    frame.render_widget(
                        Paragraph::new(Line::styled(
                            format!("{pad}{text}"),
                            if on { PROMPT_STYLE } else { DIM },
                        )),
                        row(top + rows_n + i as u16),
                    );
                }
                frame.render_widget(Paragraph::new(Line::styled(status, DIM)), row(below));
                frame.render_widget(
                    Paragraph::new(Line::styled(info, DIM)).alignment(Alignment::Right),
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
                    area.y + top + crow,
                ));
            })
            .map(|_| ())
    }

    /// Give the live region `height` rows. It keeps its bottom at the screen
    /// bottom, so growing it upward would cover the last transcript rows: they
    /// are pushed into scrollback first, which leaves blank rows for the new
    /// prompt lines.
    fn fit(&mut self, height: u16) -> io::Result<()> {
        #[cfg(test)]
        if matches!(self.term, Term::Test(_)) {
            return Ok(());
        }
        let size = self.term.size()?;
        let height = height.min(size.height).max(1);
        let area = self.term.area();
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
        self.term = Term::Real(term);
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
        #[cfg(test)]
        if matches!(self.term, Term::Test(_)) {
            return;
        }
        let area = self.term.area();
        match &mut self.term {
            Term::Real(term) => {
                let _ = execute!(
                    term.backend_mut(),
                    PopKeyboardEnhancementFlags,
                    DisableBracketedPaste,
                    cursor::MoveTo(0, area.y),
                    terminal::Clear(terminal::ClearType::FromCursorDown),
                    cursor::Show
                );
                let _ = term.flush();
            }
            #[cfg(test)]
            Term::Test(_) => {}
        }
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
        // Typing at startup is the choice: the session begins fresh and the
        // chooser's list goes with it, rather than staying in scrollback.
        if self
            .menu
            .as_ref()
            .map(|menu| menu.collapse_on_type)
            .unwrap_or(false)
        {
            self.clear_menu(false);
        }
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
        if self
            .menu
            .as_ref()
            .map(|menu| menu.collapse_on_type)
            .unwrap_or(false)
        {
            self.clear_menu(false);
        }
        let at = self.offset(self.cursor);
        self.buf.insert_str(at, &text);
        self.cursor += text.chars().count();
    }

    /// Tab on a slash line: fill in a candidate, then step to the next one on
    /// every further Tab, wrapping around. The set is captured on the first
    /// step, so the walk covers every line the typed prefix matched, not just
    /// those the completed buffer still matches. Backwards starts at the last
    /// one, as a menu does. A lone candidate finishes with a space and ends the
    /// cycle.
    fn cycle_step(&mut self, forward: bool) {
        if let Some(cycle) = &mut self.cycle {
            if cycle.matches.get(cycle.at) == Some(&self.buf) && !cycle.matches.is_empty() {
                let n = cycle.matches.len();
                cycle.at = if forward {
                    (cycle.at + 1) % n
                } else {
                    (cycle.at + n - 1) % n
                };
                self.buf = cycle.matches[cycle.at].clone();
                self.cursor = self.len();
                return;
            }
        }
        self.cycle = None;
        let matches = self.candidates();
        if matches.is_empty() {
            return;
        }
        let at = if forward { 0 } else { matches.len() - 1 };
        self.buf = matches[at].clone();
        self.cursor = self.len();
        if matches.len() == 1 {
            self.buf.push(' ');
            self.cursor = self.len();
        } else {
            self.cycle = Some(Cycle { matches, at });
        }
    }

    /// The lines the buffer is a prefix of. Only a name is completed while only
    /// a name is typed, so Tab on a bare slash word cannot land on a model; an
    /// argument is completed while it is one word, so `/model dee` matches
    /// `/model deepseek-chat`. A free-text argument takes no candidates, and a
    /// second space ends the matching.
    fn candidates(&self) -> Vec<String> {
        if !self.buf.starts_with('/') {
            return Vec::new();
        }
        match self.buf.split_once(' ') {
            None => self
                .commands
                .iter()
                .filter(|c| !c.contains(' ') && c.starts_with(&self.buf))
                .cloned()
                .collect(),
            Some((_, arg)) if !arg.contains(char::is_whitespace) => self
                .commands
                .iter()
                .filter(|c| c.starts_with(&self.buf))
                .cloned()
                .collect(),
            Some(_) => Vec::new(),
        }
    }

    /// The candidates to list under the prompt and which one the next step
    /// takes: the set the running cycle captured, or what is typed, with the
    /// first one waiting.
    fn hints(&self) -> (Vec<String>, usize) {
        match &self.cycle {
            Some(cycle) if cycle.matches.get(cycle.at) == Some(&self.buf) => {
                (cycle.matches.clone(), cycle.at)
            }
            _ => (self.candidates(), 0),
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
        while second_end > 0
            && self
                .char_at(second_end - 1)
                .is_some_and(char::is_whitespace)
        {
            second_end -= 1;
        }
        let mut second_start = second_end;
        while second_start > 0
            && self
                .char_at(second_start - 1)
                .is_some_and(|c| !c.is_whitespace())
        {
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
        while first_start > 0
            && self
                .char_at(first_start - 1)
                .is_some_and(|c| !c.is_whitespace())
        {
            first_start -= 1;
        }
        if first_start == first_end {
            return;
        }
        let first = self.buf[self.offset(first_start)..self.offset(first_end)].to_string();
        let between = self.buf[self.offset(first_end)..self.offset(second_start)].to_string();
        let second = self.buf[self.offset(second_start)..self.offset(second_end)].to_string();
        self.splice(
            first_start,
            second_end,
            &format!("{second}{between}{first}"),
        );
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
        // A control character occupies no cell on screen; draw a space so the
        // cursor column and the rendered row agree.
        let ch = if ch.is_control() { ' ' } else { ch };
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_counts_cells_not_chars() {
        assert_eq!(clip("日本語", 4), "日…");
        assert_eq!(clip("abcd", 4), "abcd");
    }

    #[test]
    fn split_drops_only_the_final_newline() {
        assert_eq!(split("a\nb\n"), vec!["a", "b"]);
        assert_eq!(split("a\n\nb"), vec!["a", "", "b"]);
    }

    #[test]
    fn prompt_layout_wraps_and_tracks_cursor() {
        let (rows, first, (row, col)) = prompt_layout(2, "ab\ncd", 5, 10, 4);
        assert_eq!(rows, vec!["ab", "cd"]);
        assert_eq!(first, 0);
        assert_eq!((row, col), (1, 2));
    }

    #[test]
    fn prompt_layout_draws_controls_as_one_cell() {
        let (rows, _, (row, col)) = prompt_layout(2, "a\u{1}b", 3, 20, 4);
        assert_eq!(rows, vec!["a b"]);
        assert_eq!((row, col), (0, 3));
    }

    #[test]
    fn wrap_breaks_at_cells() {
        let rows = wrap(Line::from("hello world"), 6);
        let text: Vec<String> = rows
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(text, vec!["hello ", "world"]);
    }

    #[test]
    fn live_region_renders_prompt_status_info_and_queue() {
        let mut screen = Screen::test_screen(40, 10);
        screen.buf = "hi".into();
        screen.cursor = 2;
        screen.set_status("Thinking…");
        screen.set_info("p/m · default");
        screen.set_queued(Some("later"));
        screen.draw().unwrap();
        let text: Vec<String> = screen
            .test_buffer()
            .content
            .chunks(40)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect())
            .collect();
        let text = text.join("\n");
        assert!(text.contains("❯ hi"), "{text}");
        assert!(text.contains("Thinking…"), "{text}");
        assert!(text.contains("p/m · default"), "{text}");
        assert!(text.contains("Queued: later"), "{text}");
    }

    #[test]
    fn editing_keys_do_not_need_a_terminal() {
        let mut screen = Screen::test_screen(40, 10);
        for c in ['a', 'b', 'c'] {
            screen.apply_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        assert_eq!(screen.buf, "abc");
        screen.apply_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        screen.apply_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(screen.buf, "ac");
        assert!(matches!(
            screen.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Action::Submit
        ));
    }

    #[test]
    fn ctrl_j_starts_a_line() {
        let mut screen = Screen::test_screen(40, 10);
        screen.apply_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
        screen.apply_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        assert_eq!(screen.buf, "\na");
    }

    #[test]
    fn chooser_renders_and_takes_keys() {
        let mut screen = Screen::test_screen(40, 12);
        screen.set_menu("Header", vec!["one".into(), "two".into()], false);
        screen.draw().unwrap();
        let text: Vec<String> = screen
            .test_buffer()
            .content
            .chunks(40)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect())
            .collect();
        let text = text.join("\n");
        assert!(text.contains("Header"), "{text}");
        assert!(text.contains("one"), "{text}");
        assert!(text.contains("two"), "{text}");
        assert!(matches!(
            screen.apply_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            Action::MenuStep(1)
        ));
        assert!(matches!(
            screen.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Action::MenuAccept
        ));
    }

    #[test]
    fn typing_at_startup_drops_the_chooser_without_a_trace() {
        let mut screen = Screen::test_screen(40, 12);
        screen.set_menu("Resume", vec!["one".into(), "two".into()], true);
        screen.insert_text("h");
        assert!(screen.menu.is_none());
        assert_eq!(screen.buf, "h");
        // No block was written: the list is gone, not reprinted above the prompt.
        assert!(screen.last.is_none());
        // `/sessions` searches instead, so its chooser waits for the line.
        screen.set_menu("Sessions", vec!["one".into()], false);
        screen.insert_text("x");
        assert!(screen.menu.is_some());
    }

    #[test]
    fn prompt_and_commit_work_without_a_terminal() {
        let mut screen = Screen::test_screen(40, 10);
        assert!(screen.is_empty());
        screen.insert_text("hello");
        assert_eq!(screen.take(), "hello");
        assert!(screen.is_empty());
        screen.insert_text("hi");
        assert_eq!(screen.commit(), "hi");
        assert!(screen.is_empty());
        screen.set_status("Thinking…");
        assert_eq!(screen.status(), "Thinking…");
        screen.set_commands(vec!["/help".into(), "/exit".into()]);
        assert_eq!(screen.commands, vec!["/help", "/exit"]);
    }

    #[test]
    fn tab_cycles_the_candidates() {
        let mut screen = Screen::test_screen(40, 10);
        screen.set_commands(vec!["/exit".into(), "/explore".into(), "/help".into()]);
        for c in ['/', 'e'] {
            screen.apply_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
        screen.apply_key(tab);
        assert_eq!(screen.buf, "/exit");
        screen.apply_key(tab);
        assert_eq!(screen.buf, "/explore");
        screen.apply_key(tab);
        assert_eq!(screen.buf, "/exit");
        // An edit ends the cycle: the next Tab starts from the text again.
        screen.apply_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE));
        assert_eq!(screen.buf, "/exitp");
        screen.apply_key(tab);
        assert_eq!(screen.buf, "/exitp");
        screen.apply_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        // One candidate left: Tab finishes it and ends the cycle.
        screen.apply_key(tab);
        assert_eq!(screen.buf, "/exit ");
        assert!(screen.cycle.is_none());
    }

    #[test]
    fn tab_finishes_a_lone_candidate_with_a_space() {
        let mut screen = Screen::test_screen(40, 10);
        screen.set_commands(vec!["/help".into(), "/exit".into()]);
        screen.insert_text("/h");
        screen.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(screen.buf, "/help ");
        assert!(screen.cycle.is_none());
        // Free text after the first argument is the user's: Tab leaves it
        // alone.
        screen.insert_text("/model");
        screen.insert_text(" a/b");
        screen.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(screen.buf, "/help /model a/b");
    }

    #[test]
    fn tab_completes_the_first_argument() {
        let mut screen = Screen::test_screen(40, 10);
        screen.set_commands(vec![
            "/model".into(),
            "/model one/big".into(),
            "/model two/small".into(),
        ]);
        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
        // A bare name completes names alone: a model line is not a prefix it
        // can land on.
        screen.insert_text("/model");
        screen.apply_key(tab);
        assert_eq!(screen.buf, "/model ");
        // Only then does the argument list itself.
        assert_eq!(
            screen.hints(),
            (vec!["/model one/big".into(), "/model two/small".into()], 0)
        );
        screen.apply_key(tab);
        assert_eq!(screen.buf, "/model one/big");
        screen.apply_key(tab);
        assert_eq!(screen.buf, "/model two/small");
        // The typed argument narrows the set; one left finishes the line.
        screen.take();
        screen.insert_text("/model tw");
        screen.apply_key(tab);
        assert_eq!(screen.buf, "/model two/small ");
        assert!(screen.cycle.is_none());
        // A second word is free text: no candidates, no fill.
        screen.take();
        screen.insert_text("/model one/big now");
        screen.apply_key(tab);
        assert_eq!(screen.buf, "/model one/big now");
    }

    #[test]
    fn arrows_step_the_candidates_like_tab() {
        let mut screen = Screen::test_screen(40, 10);
        screen.set_commands(vec!["/exit".into(), "/explore".into(), "/help".into()]);
        screen.insert_text("/e");
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        // Backwards starts at the last candidate, forwards at the first.
        screen.apply_key(key(KeyCode::Up));
        assert_eq!(screen.buf, "/explore");
        screen.apply_key(key(KeyCode::Down));
        assert_eq!(screen.buf, "/exit");
        screen.apply_key(key(KeyCode::Down));
        assert_eq!(screen.buf, "/explore");
        screen.apply_key(key(KeyCode::Up));
        assert_eq!(screen.buf, "/exit");
        // Nothing to complete: the arrows leave the line alone.
        screen.take();
        screen.insert_text("hello");
        screen.apply_key(key(KeyCode::Up));
        assert_eq!(screen.buf, "hello");
    }

    #[test]
    fn hints_are_listed_under_the_prompt() {
        let mut screen = Screen::test_screen(20, 20);
        screen.set_commands(vec![
            "/skills".into(),
            "/skill:ship".into(),
            "/sessions".into(),
        ]);
        screen.insert_text("/sk");
        let (hints, at) = screen.hints();
        assert_eq!(hints, vec!["/skills", "/skill:ship"]);
        assert_eq!(at, 0);
        screen.draw().unwrap();
        let rows: Vec<String> = screen
            .test_buffer()
            .content
            .chunks(20)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect())
            .collect();
        // One blank cell of margin, then the prompt, then the list under its
        // text.
        assert_eq!(rows[2].trim_end(), " ❯ /sk");
        assert_eq!(rows[3].trim_end(), "   /skills");
        assert_eq!(rows[4].trim_end(), "   /skill:ship");
        // The one the next step takes is the bright one.
        let cell = |x: u16, y: u16| screen.test_buffer()[(x, y)].clone();
        assert_eq!(cell(3, 3).fg, Color::Cyan);
        assert_ne!(cell(3, 4).fg, Color::Cyan);
        // After a step the whole set stays listed, with the buffer on the
        // second one.
        screen.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(screen.buf, "/skills");
        assert_eq!(
            screen.hints(),
            (vec!["/skills".into(), "/skill:ship".into()], 0)
        );
        screen.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(screen.buf, "/skill:ship");
        assert_eq!(screen.hints().1, 1);
    }

    #[test]
    fn short_terminal_lists_fewer_candidates() {
        let mut screen = Screen::test_screen(20, 14);
        screen.set_commands(vec!["/c1".into(), "/c2".into(), "/c3".into()]);
        screen.insert_text("/c");
        screen.draw().unwrap();
        let rows: Vec<String> = screen
            .test_buffer()
            .content
            .chunks(20)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect())
            .collect();
        // Half the screen, less the chrome and the prompt, is one row: the
        // live region stays inside its cap and the rest of the list is cut.
        assert_eq!(rows[3].trim_end(), "   /c1");
        assert_eq!(rows[4].trim_end(), "");
    }

    #[test]
    fn hint_window_follows_the_selection() {
        let mut screen = Screen::test_screen(20, 14);
        let commands: Vec<String> = (1..=7).map(|i| format!("/c{i}")).collect();
        screen.set_commands(commands);
        screen.insert_text("/c");
        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
        for _ in 0..7 {
            screen.apply_key(tab);
        }
        assert_eq!(screen.buf, "/c7");
        screen.draw().unwrap();
        let text: Vec<String> = screen
            .test_buffer()
            .content
            .chunks(20)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect())
            .collect();
        let text = text.join("\n");
        assert!(text.contains("   /c7"), "{text}");
        assert!(!text.contains("   /c1"), "{text}");
        assert!(!text.contains("   /c2"), "{text}");
    }

    #[test]
    fn reset_starts_the_transcript_over() {
        let mut screen = Screen::test_screen(40, 12);
        // Collected but not yet laid out: without the reset it would join the
        // new session's first block.
        screen.text("stale paragraph");
        screen.set_menu("Header", vec!["one".into()], false);
        screen.set_status("Thinking…");
        screen.set_queued(Some("later"));
        screen.reset();
        assert!(screen.menu.is_none());
        assert!(screen.status().is_empty());
        assert!(screen.last.is_none());
        // Nothing is left to lay out: a flush finds an empty stream and writes
        // no block, so the new session's first text has no separator to skip.
        screen.flush();
        assert!(screen.last.is_none());
    }

    #[test]
    fn transcript_writers_do_not_need_a_terminal() {
        let mut screen = Screen::test_screen(40, 10);
        screen.insert(vec![Line::from("tool")], Block::Tool);
        screen.fail("nope");
        screen.output("binary text\n");
        screen.error("red text\n");
        screen.echo_user("user **markdown**");
        screen.flush();
    }
}
