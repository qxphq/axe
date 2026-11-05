//! The screen: a live frame at the bottom of the terminal, the transcript above
//! it.
//!
//! `ratatui` owns the arithmetic that used to live here: where the cursor goes,
//! what to repaint after a resize, how to push a line into scrollback without
//! losing the drawing. This module decides what the two zones are, and one rule
//! keeps them apart:
//!
//! *The frame is as tall as what it shows.* Its rows are the prompt's wrapped
//! lines, the queued line, the candidates, and the status with its two blank
//! rows — `Frame::layout` — so an idle prompt sits directly under the last
//! block. A chooser is the exception: it owns a window of the terminal's size
//! while it is open. Reserved rows that show nothing are rows taken from the
//! transcript for good, which is why the frame follows its content.
//!
//! *A block goes where the transcript ends.* A terminal appends rows at the
//! bottom and cannot take them back, so a frame that shrank leaves blank rows
//! above it. `Screen::slack` counts them, and `write` fills them from the top:
//! a block lands directly under the block before it whatever the frame is
//! doing, and the rows left over sit above the prompt, where a blank row reads
//! as space rather than as a separator between two messages.

use std::io::{self, IsTerminal, Stdout, Write};

#[cfg(test)]
use ratatui::backend::TestBackend;
use ratatui::backend::{Backend, ClearType, CrosstermBackend, WindowSize};
use ratatui::buffer::{Buffer as Cells, Cell};
use ratatui::crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, KeyCode, KeyEvent, KeyModifiers,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::{cursor, execute, terminal};
use ratatui::layout::{Alignment, Position, Rect, Size};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use ratatui::{Frame as TermFrame, Terminal, TerminalOptions, Viewport};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::editor::Buffer;
use crate::markdown;

/// Blank cells at each screen edge: nothing is written against the border.
const PAD: u16 = 1;

/// Rows of transcript a list may never take: the frame is at the bottom, but
/// the conversation has to stay readable through it.
const MIN_TRANSCRIPT: u16 = 3;

const PROMPT_STYLE: Style = Style::new().fg(Color::Cyan);
/// Explore mode's prompt: the glyph says the tool set is read-only.
const EXPLORE_STYLE: Style = Style::new().fg(Color::Yellow);
/// Marker for an already committed line in the transcript.
const MARK: &str = "█";
const DIM: Style = Style::new().add_modifier(Modifier::DIM);
const ERROR_STYLE: Style = Style::new().fg(Color::Red);
/// A caution, never a failure: a question waiting for an answer, a tool set
/// that cannot write, a window that is nearly full.
const CAUTION: Style = Style::new().fg(Color::Yellow);

/// The cells between two pieces of the info row.
pub const SEP: &str = "  ·  ";

/// The info row as one plain line, for the paths that have no screen to color.
pub fn info_text(parts: &[(Info, String)]) -> String {
    let texts: Vec<&str> = parts.iter().map(|(_, text)| text.as_str()).collect();
    texts.join(SEP)
}

/// A piece of the info row. The caller says what a piece is, not how it looks:
/// every color below is one of the terminal's own ANSI names, so the theme the
/// user chose decides what it actually is.
#[derive(Clone, Copy, PartialEq)]
pub enum Info {
    /// The session's name. Prose, so it keeps the terminal's default color.
    Title,
    /// `provider/model`: the same cyan as the prompt, because it is the same
    /// thing, what this session is talking to.
    Selection,
    /// The thinking level: a knob next to the selection, so it gets its own
    /// color rather than sharing one.
    Level,
    /// The window, and how full it is when the provider counted one. The only
    /// piece whose color means something: green, then yellow, then red.
    Fill(Option<u8>),
    /// A mode that takes tools away.
    Mode,
}

impl Info {
    fn style(self) -> Style {
        match self {
            Info::Title => Style::new(),
            Info::Selection => PROMPT_STYLE,
            Info::Level => Style::new().fg(Color::Magenta),
            Info::Fill(None) => DIM,
            Info::Fill(Some(full)) => match full {
                ..70 => Style::new().fg(Color::Green),
                70..90 => CAUTION,
                _ => ERROR_STYLE,
            },
            Info::Mode => CAUTION,
        }
    }
}

/// Whether `block` needs a blank row above it: everything but two tool calls in
/// a row, which are one block. Both writers place it, so a transcript reads the
/// same whether it has a screen or a pipe.
pub fn separated(last: Option<Block>, block: Block) -> bool {
    !(block == Block::Tool && last == Some(block))
}

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

/// The candidates under the prompt and the row the marker is on. The marker
/// moves without touching the line, so the list is captured once against the
/// line it was taken from: `typed` holds that line, and an edit ends it. Being
/// captured also means Tab walks every candidate the prefix matched, not just
/// those the line it wrote last still matches.
struct Selection {
    typed: String,
    matches: Vec<String>,
    at: usize,
}

impl Selection {
    /// The list belongs to the line it was taken from: an edit ends it.
    fn belongs(&self, line: &str) -> bool {
        self.typed == line
    }
}

/// What the live frame is for. A chooser is a different frame, not a taller
/// one: the mode, never the content, decides the frame's shape.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// The prompt, with the candidates for what is typed in the rows above it.
    Idle,
    /// A chooser: its header and entries take the rows the candidates would.
    Choosing,
}

/// Where the live frame's parts sit, in screen rows. The parts carry rows, not
/// the width, which is the terminal's and comes off the sides in `draw`, and
/// nothing is written against the screen's bottom edge.
///
/// The frame is as tall as the rows it shows, and it carries the blank rows it
/// keeps around the prompt and the status: one above the prompt, one between
/// the prompt and the status, one under the status, however many rows the
/// rest takes. A frame sized by the terminal instead of by its content would
/// sit on rows it does not use, and those rows are the transcript's.
#[derive(Clone, Copy)]
struct Frame {
    /// Candidates, or a chooser's header and its entries. Blank when unused.
    list: Rect,
    /// The line waiting for the turn boundary: one row, never more, and none at
    /// all while nothing is waiting.
    queued: Rect,
    /// The prompt. It scrolls inside these rows, so a buffer longer than the
    /// three rows it composes in does not move anything.
    prompt: Rect,
    /// What the turn is doing on the left, what the session points at on the
    /// right, or the one question that outranks both.
    status: Rect,
}

/// The rows the frame asks for above its own: the prompt's wrapped rows, the
/// queued line, and the candidates. Clamped by `Frame::layout`, which decides
/// how many of them the terminal can spare.
#[derive(Clone, Copy, Default)]
struct Shown {
    prompt: u16,
    queued: bool,
    hints: u16,
}

impl Frame {
    /// The frame for a terminal this tall, its top row at zero: the shape, with
    /// `at` putting it where the terminal has it.
    ///
    /// The height follows `shown`, not the terminal: a prompt that wraps asks
    /// for a row and gets it, and gives it back when the line is done. A
    /// chooser is the one part sized by the screen — it is a window over the
    /// transcript, and its rows are committed when it closes.
    fn layout(height: u16, mode: Mode, shown: Shown) -> Frame {
        let height = height.max(1);
        // The prompt composes in up to three rows and scrolls inside them: one
        // row is always there, because a prompt with no row is no prompt.
        let prompt_rows = shown.prompt.clamp(1, 3);
        let queued_rows = u16::from(shown.queued);
        // Blank row, prompt, blank row, status row, blank row under it: the
        // frame carries them whether or not it has anything to say, so the
        // prompt has a row of its own above it, the status never touches the
        // prompt, and neither touches the bottom edge.
        let fixed = queued_rows + prompt_rows + 4;
        // The transcript keeps its own rows on a short terminal, whatever is
        // asking for space.
        let room = height.saturating_sub(fixed + MIN_TRANSCRIPT);
        let list_rows = match mode {
            Mode::Idle => shown.hints.min((height / 12).clamp(1, 4)).min(room),
            // A header, then entries: a chooser is worth at least two rows.
            Mode::Choosing => (height / 3).clamp(2, 10).min(room),
        };
        let rows = (fixed + list_rows).min(height);
        let area = Rect::new(0, 0, 0, rows);
        // Top down: the parts are laid end to end, so a part that is not shown
        // takes its row out of the frame and not out of the blank row above the
        // prompt, which is the frame's own and always there.
        let list = Rect {
            y: 0,
            height: list_rows,
            ..area
        };
        let queued = Rect {
            y: list_rows,
            height: queued_rows,
            ..area
        };
        let status = Rect {
            y: rows.saturating_sub(2),
            height: 1,
            ..area
        };
        let prompt = Rect {
            y: list_rows + queued_rows + 1,
            height: prompt_rows.min(status.y.saturating_sub(list_rows + queued_rows + 1)),
            ..area
        };
        Frame {
            list,
            queued,
            prompt,
            status,
        }
    }

    /// The same parts, `top` rows down the screen.
    fn at(&self, top: u16) -> Frame {
        let shift = |part: Rect| Rect {
            y: part.y + top,
            ..part
        };
        Frame {
            list: shift(self.list),
            queued: shift(self.queued),
            prompt: shift(self.prompt),
            status: shift(self.status),
        }
    }

    /// The rows the frame takes: the status row and the blank row under it.
    fn height(&self) -> u16 {
        self.status.y + self.status.height + 1
    }

    /// The row the cursor sits on inside the prompt, giving the text the bottom
    /// of its box: a one-line buffer leaves the frame above it untouched, and a
    /// longer one grows upward.
    fn prompt_top(&self, rows: u16) -> u16 {
        self.prompt.bottom().saturating_sub(rows).max(self.prompt.y)
    }
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

    fn draw<F: FnOnce(&mut TermFrame)>(&mut self, draw: F) -> io::Result<()> {
        match self {
            Term::Real(term) => term.draw(draw).map(|_| ()),
            #[cfg(test)]
            Term::Test(term) => term.draw(draw).map(|_| ()),
        }
    }

    fn insert_before<F: FnOnce(&mut Cells)>(&mut self, height: u16, draw: F) -> io::Result<()> {
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

    /// Draw dense rows at an absolute screen position above the viewport: the
    /// rows a shrunk frame left blank. Nothing above the frame is in the
    /// terminal's back buffer, so this goes through the backend, which is what
    /// `insert_before` does with the block it is given.
    fn paint_above(
        &mut self,
        top: u16,
        rows: Vec<Line<'static>>,
        width: u16,
        margin: u16,
    ) -> io::Result<()> {
        match self {
            Term::Real(term) => paint_above(term.backend_mut(), top, rows, width, margin),
            #[cfg(test)]
            Term::Test(term) => paint_above(term.backend_mut(), top, rows, width, margin),
        }
    }

    /// The same terminal, in a viewport `height` rows tall starting at the
    /// cursor. The backend is carried over, so a test's scrollback survives the
    /// move: it is the session's history, not the terminal's.
    fn reshape(&mut self, height: u16) -> io::Result<()> {
        let options = || TerminalOptions {
            viewport: Viewport::Inline(height),
        };
        let term = match self {
            // Deriving the area asks the terminal where its cursor is, and
            // crossterm answers only while it holds the event reader: the input
            // thread takes it in short turns, so this can miss a few times
            // before it lands.
            Term::Real(_) => {
                let mut tries = 0;
                loop {
                    match Terminal::with_options(
                        Tail(CrosstermBackend::new(io::stdout())),
                        options(),
                    ) {
                        Ok(term) => break Term::Real(term),
                        Err(_) if tries < 50 => {
                            tries += 1;
                            std::thread::sleep(std::time::Duration::from_millis(2));
                        }
                        Err(e) => return Err(e),
                    }
                }
            }
            #[cfg(test)]
            Term::Test(term) => {
                Term::Test(Terminal::with_options(term.backend().clone(), options())?)
            }
        };
        *self = term;
        Ok(())
    }
}

pub struct Screen {
    term: Term,
    /// Plain text: the editor owns the color.
    prompt: String,
    /// Color of the prompt glyph, switched by explore mode.
    prompt_style: Style,
    /// The line being typed, its cursor and the last killed text.
    ed: Buffer,
    /// Left half of the row under the prompt: thinking, or what is running.
    status: String,
    /// A question the user must answer, on the row under the prompt with the
    /// info row cleared: the status is metadata and can be cut, this cannot.
    /// The ESC offer is the only one.
    alert: Option<String>,
    /// The line submitted mid-turn, waiting for the boundary. Its own row,
    /// above the prompt: the queue grows where the user is about to type, and
    /// a queued line that scrolls away silently looks like the Enter was lost.
    queued: Option<String>,
    /// Right half of the row under the prompt, permanent: what this session is
    /// pointed at. Set by the caller, which owns the model, the thinking level
    /// and the window, and colored here.
    info: Vec<(Info, String)>,
    /// The lines Tab completes, in the caller's spelling: a slash command on
    /// its own, or one with its first argument filled in.
    commands: Vec<String>,
    /// The candidates the line put under the prompt, if any: the arrows move
    /// its marker and Enter runs what they moved to.
    selection: Option<Selection>,
    /// The last block written, for the blank line between blocks.
    last: Option<Block>,
    /// Rows between the end of the transcript and the top of the frame that are
    /// known to be blank. `settle` is the only writer, when the frame gives rows
    /// back; `write` fills them from the top, so a block always lands under the
    /// block before it. See the module note.
    slack: u16,
    /// Markdown being collected. A block is laid out once it ends, not once
    /// per line: a table needs its whole header, a paragraph its last line.
    md: markdown::Stream,
    /// Reasoning, collected the same way. Its own stream: a thought's last
    /// paragraph must not run into the answer's first.
    thought: markdown::Stream,
    /// A chooser above the prompt, when one is open.
    menu: Option<Menu>,
    restored: bool,
    /// State changed since the last frame. The loops paint; the setters only
    /// mark, so a change made while nobody is drawing cannot be lost.
    dirty: bool,
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
        // The frame the session starts in; `draw` keeps it there. A terminal
        // shorter than the frame must still start.
        let (_, height) = terminal::size().unwrap_or((80, 24));
        let rows = Frame::layout(height, Mode::Idle, Shown::default())
            .height()
            .clamp(1, height.max(1));
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
            ed: Buffer::default(),
            status: String::new(),
            alert: None,
            queued: None,
            info: Vec::new(),
            commands: Vec::new(),
            selection: None,
            last: None,
            slack: 0,
            md: markdown::Stream::new(),
            thought: markdown::Stream::new(),
            menu: None,
            restored: false,
            dirty: true,
        })
    }

    /// A screen shaped like a real one: an inline region at the bottom of the
    /// terminal, the transcript above it, and a scrollback the backend keeps.
    /// Everything `fit` does to a real terminal it does here too.
    #[cfg(test)]
    fn test_screen(width: u16, height: u16) -> Screen {
        let rows = Frame::layout(height, Mode::Idle, Shown::default())
            .height()
            .clamp(1, height);
        let mut backend = TestBackend::new(width, height);
        backend
            .set_cursor_position(Position::new(0, height - rows))
            .unwrap();
        let term = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(rows),
            },
        )
        .unwrap();
        Screen {
            term: Term::Test(term),
            prompt: "❯ ".to_string(),
            prompt_style: PROMPT_STYLE,
            ed: Buffer::default(),
            status: String::new(),
            alert: None,
            queued: None,
            info: Vec::new(),
            commands: Vec::new(),
            selection: None,
            last: None,
            slack: 0,
            md: markdown::Stream::new(),
            thought: markdown::Stream::new(),
            menu: None,
            restored: false,
            dirty: true,
        }
    }

    #[cfg(test)]
    fn is_dirty(&self) -> bool {
        self.dirty
    }

    #[cfg(test)]
    fn test_buffer(&self) -> &Cells {
        match &self.term {
            Term::Test(term) => term.backend().buffer(),
            Term::Real(_) => unreachable!(),
        }
    }

    /// Everything a terminal would have after this session: the rows that
    /// scrolled off, then the rows above the live frame, oldest first. Trailing
    /// space goes, and so do blank rows at either end: the frame's margin above
    /// the first block and the space left below the last one are not transcript.
    ///
    /// This is what a test asserts on. What is still on screen is only the tail
    /// of it.
    #[cfg(test)]
    fn transcript(&mut self) -> Vec<String> {
        let top = self.term.area().top() as usize;
        let Term::Test(term) = &self.term else {
            unreachable!()
        };
        let buffer = term.backend().buffer();
        let width = buffer.area.width.max(1) as usize;
        let mut rows: Vec<String> = term
            .backend()
            .scrollback()
            .content
            .chunks(width)
            .map(row_text)
            .collect();
        rows.extend(buffer.content.chunks(width).take(top).map(row_text));
        while rows.last().is_some_and(|row| row.is_empty()) {
            rows.pop();
        }
        rows.drain(..rows.iter().take_while(|row| row.is_empty()).count());
        rows
    }

    /// Explore mode colors the prompt; the transcript keeps its own palette.
    pub fn set_explore(&mut self, explore: bool) {
        self.prompt_style = if explore { EXPLORE_STYLE } else { PROMPT_STYLE };
        self.dirty = true;
    }

    pub fn status(&self) -> &str {
        &self.status
    }

    pub fn set_status(&mut self, text: &str) {
        self.status = text.to_string();
        self.dirty = true;
    }

    /// A question the user must answer. It owns the row under the prompt and
    /// the info row steps aside for as long as it is up: an offer nobody can
    /// read is not an offer.
    pub fn set_alert(&mut self, text: Option<&str>) {
        self.alert = text.map(str::to_string);
        self.dirty = true;
    }

    pub fn set_queued(&mut self, line: Option<&str>) {
        self.queued = line.map(str::to_string);
        self.dirty = true;
    }

    /// The info row, piece by piece. The separator and the colors are the
    /// screen's; the caller only says what each piece is.
    pub fn set_info(&mut self, parts: impl IntoIterator<Item = (Info, String)>) {
        self.info = parts.into_iter().collect();
        self.dirty = true;
    }

    /// The lines Tab completes: slash names, and names with their first
    /// argument.
    pub fn set_commands(&mut self, commands: Vec<String>) {
        self.commands = commands;
        self.dirty = true;
    }

    /// Open a chooser. Rows are plain text; the marker is drawn on the
    /// selected one.
    pub fn set_menu(&mut self, header: &str, rows: Vec<String>, collapse_on_type: bool) {
        self.dirty = true;
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
        self.dirty = true;
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
        self.dirty = true;
        let Some(menu) = &mut self.menu else { return };
        let last = menu.rows.len().saturating_sub(1);
        menu.selected = menu.selected.saturating_add_signed(delta).min(last);
    }

    /// A chooser is active only when the last draw had room for it. An open
    /// menu on a short screen must not eat Enter or the arrows.
    fn menu_active(&self) -> bool {
        self.menu.as_ref().is_some_and(|menu| menu.visible > 0)
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
        self.ed.buf.is_empty()
    }

    /// The completed line, leaving the buffer empty.
    pub fn take(&mut self) -> String {
        self.dirty = true;
        self.selection = None;
        self.ed.take()
    }

    /// Fold one key into the buffer. Holds no interrupt semantics: ESC belongs
    /// to the caller, which knows whether a turn is in flight.
    pub fn apply_key(&mut self, key: KeyEvent) -> Action {
        self.dirty = true;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            // A chooser owns the arrows and Esc while it is open. Enter on an
            // empty line accepts it; Enter on a search query submits as usual.
            KeyCode::Up if self.menu_active() => return Action::MenuStep(-1),
            KeyCode::Down if self.menu_active() => return Action::MenuStep(1),
            KeyCode::Esc if self.menu.is_some() => return Action::MenuCancel,
            KeyCode::Enter if self.menu_active() && self.ed.buf.is_empty() => {
                return Action::MenuAccept
            }
            // A new line, not a submission: Alt is the ESC-prefixed form every
            // terminal sends. Shift+Enter is deliberately not a binding: a
            // terminal reports it as a bare Enter unless the kitty protocol
            // says otherwise, and asking for that costs every shifted
            // character its layout.
            KeyCode::Enter if alt => self.insert_char('\n'),
            // Enter runs what the marker was moved to. A list nobody stepped
            // through is a preview of Tab, so the line submits as typed.
            KeyCode::Enter => {
                self.take_marked();
                return Action::Submit;
            }
            KeyCode::Backspace if alt || ctrl => self.ed.kill_word_back(),
            KeyCode::Backspace => self.ed.backspace(),
            KeyCode::Delete => self.ed.delete(),
            KeyCode::Left if alt || ctrl => self.ed.word_back(),
            KeyCode::Right if alt || ctrl => self.ed.word_forward(),
            KeyCode::Left => self.ed.cursor = self.ed.cursor.saturating_sub(1),
            KeyCode::Right => self.ed.cursor = (self.ed.cursor + 1).min(self.ed.len()),
            KeyCode::Home => self.ed.cursor = 0,
            KeyCode::End => self.ed.cursor = self.ed.len(),
            KeyCode::Tab => self.complete(),
            KeyCode::Up => self.mark(-1),
            KeyCode::Down => self.mark(1),
            KeyCode::BackTab => return Action::Explore,
            // A shell's Ctrl-C: drop what was typed, keep the prompt.
            KeyCode::Char('c') if ctrl => {
                self.ed.buf.clear();
                self.ed.cursor = 0;
            }
            KeyCode::Char('d') if ctrl && self.ed.buf.is_empty() => return Action::Eof,
            KeyCode::Char('d') if ctrl => self.ed.delete(),
            KeyCode::Char('a') if ctrl => self.ed.cursor = 0,
            KeyCode::Char('e') if ctrl => self.ed.cursor = self.ed.len(),
            KeyCode::Char('b') if ctrl => self.ed.cursor = self.ed.cursor.saturating_sub(1),
            KeyCode::Char('f') if ctrl => self.ed.cursor = (self.ed.cursor + 1).min(self.ed.len()),
            KeyCode::Char('b') if alt => self.ed.word_back(),
            KeyCode::Char('f') if alt => self.ed.word_forward(),
            KeyCode::Char('w') if ctrl || alt => self.ed.kill_word_back(),
            KeyCode::Char('d') if alt => self.ed.kill_word_forward(),
            KeyCode::Char('u') if ctrl => self.ed.kill_to_start(),
            KeyCode::Char('k') if ctrl => self.ed.kill_to_end(),
            KeyCode::Char('y') if ctrl => self.ed.yank(),
            KeyCode::Char('t') if ctrl => self.ed.transpose_chars(),
            KeyCode::Char('t') if alt => self.ed.transpose_words(),
            // Ctrl+J is the newline byte, and what a terminal-side Shift+Enter
            // mapping sends. One buffer character, not a submission.
            KeyCode::Char('j') if ctrl => self.insert_char('\n'),
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
        self.echo(&line);
        self.status.clear();
        self.alert = None;
        line
    }

    /// A user line from somewhere else: the resumed transcript, or one the turn
    /// took mid-way. It is complete already, so the same layout as a committed
    /// line, without a buffer.
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

    /// A chooser is not the prompt's frame but its own.
    fn mode(&self) -> Mode {
        if self.menu.is_some() {
            Mode::Choosing
        } else {
            Mode::Idle
        }
    }

    /// The cells the prompt's glyph takes at the left of every row.
    fn gutter(&self) -> usize {
        self.prompt.chars().map(|c| c.width().unwrap_or(1)).sum()
    }

    /// The prompt laid out at this width: its rows, and the cursor as a row and
    /// a column after the glyph. The frame is sized from the count, so a line
    /// that wraps gets the rows it fills and gives them back when it is done.
    fn prompt_rows(&self, gutter: usize, cols: usize) -> (Vec<String>, (u16, u16)) {
        prompt_layout(gutter, &self.ed.buf, self.ed.cursor, cols)
    }

    /// The rows the frame has to show, with a prompt of `prompt` rows: what the
    /// frame is as tall as. A chooser's entries are its own window, so they do
    /// not count here.
    fn shown(&self, prompt: usize) -> Shown {
        let hints = if self.menu.is_some() {
            0
        } else {
            self.hints().0.len()
        };
        Shown {
            prompt: u16::try_from(prompt).unwrap_or(u16::MAX),
            queued: self.queued.is_some(),
            hints: u16::try_from(hints).unwrap_or(u16::MAX),
        }
    }

    /// Where the frame is, as the next `draw` will put it. The tests read a
    /// part's rows from here, so what they inspect is what was drawn.
    #[cfg(test)]
    fn frame(&mut self) -> Frame {
        let cols = self.cols();
        let (rows, _) = self.prompt_rows(self.gutter(), cols);
        let layout = Frame::layout(
            self.term.size().unwrap().height,
            self.mode(),
            self.shown(rows.len()),
        );
        layout.at(self.term.area().y)
    }

    /// Put a block into the transcript. The only writer of rows above the
    /// frame, together with `settle`: it is here that a block is placed where
    /// the transcript *ends*, not where the frame begins, so the blank rows a
    /// shrunk frame left behind end up below the block and never between it and
    /// the one before.
    fn write(&mut self, lines: Vec<Line<'static>>, block: Block) {
        self.dirty = true;
        // Whatever follows a thought ends it, and the thought goes first.
        if block != Block::Thought {
            self.end_thought();
        }
        let mut lines = lines;
        // The separator belongs to the writer: a caller's own trailing blank
        // rows would be a second one, and a block that is nothing but blanks is
        // not a block at all.
        while lines.last().is_some_and(blank) {
            lines.pop();
        }
        if lines.is_empty() {
            return;
        }
        // Consecutive tool calls are one block: they belong together. A user
        // line is its own, and markdown arrives one laid-out block at a time,
        // so neither continues.
        let continues = !separated(self.last, block);
        if !continues {
            lines.insert(0, Line::default());
        }
        self.last = Some(block);
        let width = self.term.size().map(|s| s.width).unwrap_or(80).max(1) as usize;
        // Wrap and render inside the margin, so nothing lands on the edge.
        let margin = PAD.min(width.saturating_sub(1) as u16);
        let cols = width.saturating_sub(2 * margin as usize).max(1);
        let rows: Vec<Line<'static>> = lines
            .into_iter()
            .flat_map(|line| wrap(line, cols))
            .collect();
        let total = rows.len() as u16;
        // The rows a frame gave up are blank and already above it: the top of
        // the block goes there, and only what does not fit is inserted next to
        // the frame. Either way the block starts at `frame.top - max(total,
        // slack)`, which is exactly under the block before it.
        let fits = self.slack.min(total);
        let mut above = rows;
        let rest = above.split_off(fits as usize);
        if total > fits {
            let _ = self
                .term
                .insert_before(total - fits, |buf| paint(buf, rest, margin));
        }
        if fits > 0 {
            let top = self.term.area().top().saturating_sub(total.max(self.slack));
            let _ = self.term.paint_above(top, above, width as u16, margin);
            self.slack -= fits;
        }
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
        self.dirty = true;
        // Collected markdown would otherwise join the new session's first
        // block; the transcript starts with no block to continue.
        self.md = markdown::Stream::new();
        self.thought = markdown::Stream::new();
        self.last = None;
        self.slack = 0;
        self.menu = None;
        self.status.clear();
        self.alert = None;
        self.queued = None;
        match &mut self.term {
            Term::Real(term) => {
                // `terminal::Clear(All)` only reaches the screen; what scrolled
                // off is in the terminal's own history, which `Purge` drops.
                let _ = execute!(
                    term.backend_mut(),
                    terminal::Clear(terminal::ClearType::All),
                    terminal::Clear(terminal::ClearType::Purge)
                );
            }
            #[cfg(test)]
            Term::Test(_) => {}
        }
        // The screen above the frame is gone, so the frame stands on nothing:
        // it goes back to the bottom of an empty terminal.
        let _ = self.bottom();
    }

    /// Put the frame at the bottom of the terminal, as `Screen::new` leaves it,
    /// and blank it. Used where nothing is above the frame any more.
    fn bottom(&mut self) -> io::Result<()> {
        let size = self.term.size()?;
        let height = self.term.area().height;
        let top = size.height.saturating_sub(height);
        self.term.set_cursor_position(Position::new(0, top))?;
        self.term.reshape(height)?;
        self.term.clear()
    }

    /// Repaint the live frame.
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
        let gutter = self.gutter();
        let mode = self.mode();
        // The frame is as tall as what it shows, and the prompt's own rows are
        // part of that: a line that wraps asks for the row it fills.
        let (rows, (crow, ccol)) = self.prompt_rows(gutter, cols);
        let layout = Frame::layout(size.height, mode, self.shown(rows.len()));
        self.settle(layout.height())?;
        // The frame is the live area the last write left us, not a rect derived
        // from the terminal: an insert walks it down the screen until it
        // reaches the bottom, and the parts follow it.
        let layout = layout.at(self.term.area().y);
        // The cursor is the frame's: a buffer longer than the rows it composes
        // in scrolls to keep it, from the bottom of the box up.
        let first = (crow as usize + 1).saturating_sub(layout.prompt.height.max(1) as usize);
        let visible = &rows[first.min(rows.len())..];
        let top = layout.prompt_top(visible.len() as u16);
        // A chooser's rows are its header and its entries; the window follows
        // the selection so the one the next step takes is on screen. Zero
        // entries means the frame has no room for a chooser at all, and its
        // keys must not act on an invisible list.
        let entries = layout.list.height.saturating_sub(1) as usize;
        self.menu_follow(entries);
        if let Some(menu) = &mut self.menu {
            menu.visible = entries;
        }
        let menu = self.menu.as_ref().filter(|_| entries > 0).map(|menu| {
            let start = menu.scroll;
            let end = (start + entries).min(menu.rows.len());
            (
                menu.header.clone(),
                menu.rows[start..end].to_vec(),
                menu.selected,
                menu.scroll,
            )
        });
        // The candidates take the list rows while nothing is being chosen, the
        // window following the selection so the one the next step takes is on
        // screen. They sit above the prompt, bottom row first, so the next
        // candidate is always the one nearest what is being typed.
        let (hints, at) = match menu {
            Some(_) => (Vec::new(), 0),
            None => self.hints(),
        };
        let fit = hints.len().min(layout.list.height as usize);
        let start = if fit == 0 {
            0
        } else {
            at.saturating_sub(fit - 1).min(hints.len() - fit)
        };
        let hints = &hints[start..];
        let hints = &hints[..hints.len().min(fit)];
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
        // a continuation of what is being typed. The marked row is the bright
        // one: Enter runs it, Tab puts it on the line.
        let pad = " ".repeat(gutter.min(cols.saturating_sub(1)));
        let queued = self.queued.clone();
        // The row is shared: the status says what the turn is doing, the info
        // what the session is pointed at. Both are clipped, and the info never
        // takes more than half of it — a title long enough to fill the row used
        // to squeeze the status to nothing, so a running turn looked idle. An
        // alert is the exception: it is a question, so it takes the row and the
        // info waits.
        let info = info_line(&self.info, cols / 2);
        let gap = usize::from(info.width() > 0);
        let status = clip(&self.status, cols.saturating_sub(info.width() + gap));

        let frame = self
            .term
            .draw(|frame| {
                // A part is a row range: the frame carries rows, the terminal
                // carries the width and the margin. A part outside the screen
                // is dropped rather than drawn: `settle` has just made the two
                // agree, and the half second a resize takes must not panic.
                let screen = frame.area();
                let side = Rect {
                    x: screen.x + margin,
                    width: screen.width.saturating_sub(2 * margin),
                    ..screen
                };
                let part = |r: Rect| {
                    Rect {
                        y: r.y,
                        height: r.height,
                        ..side
                    }
                    .intersection(screen)
                };
                let line = |y: u16| {
                    Rect {
                        y,
                        height: 1,
                        ..side
                    }
                    .intersection(screen)
                };
                if let Some((header, entries, selected, scroll)) = &menu {
                    frame.render_widget(
                        Paragraph::new(Line::styled(clip(header, cols), DIM)),
                        line(layout.list.y),
                    );
                    for (i, entry) in entries.iter().enumerate() {
                        let on = *selected == scroll + i;
                        let marker = if on { "▸ " } else { "  " };
                        let text = clip(entry, cols.saturating_sub(2));
                        frame.render_widget(
                            Paragraph::new(Line::styled(
                                format!("{marker}{text}"),
                                if on { PROMPT_STYLE } else { DIM },
                            )),
                            line(layout.list.y + 1 + i as u16),
                        );
                    }
                }
                // Bottom-aligned in its rows, so a suggestion sits against the
                // prompt it completes.
                let first_hint = layout.list.bottom() - hints.len() as u16;
                for (i, hint) in hints.iter().enumerate() {
                    let on = start + i == at;
                    let text = clip(hint, cols.saturating_sub(pad.len()));
                    frame.render_widget(
                        Paragraph::new(Line::styled(
                            format!("{pad}{text}"),
                            if on { PROMPT_STYLE } else { DIM },
                        )),
                        line(first_hint + i as u16),
                    );
                }
                frame.render_widget(
                    Paragraph::new(prompt_rows),
                    Rect {
                        y: top,
                        height: visible.len() as u16,
                        ..part(layout.prompt)
                    },
                );
                let label = match &queued {
                    // One row: a queued message's own line breaks would be lost.
                    Some(line) => clip(&format!("Queued: {}", line.replace('\n', " ")), cols),
                    None => String::new(),
                };
                frame.render_widget(
                    Paragraph::new(Line::styled(label, DIM)),
                    line(layout.queued.y),
                );
                // One row for the question, whole: the info row is metadata
                // and comes back when the question is answered.
                match &self.alert {
                    Some(text) => frame.render_widget(
                        Paragraph::new(Line::styled(clip(text, cols), CAUTION)),
                        line(layout.status.y),
                    ),
                    None => {
                        frame.render_widget(
                            Paragraph::new(Line::styled(status, DIM)),
                            line(layout.status.y),
                        );
                        frame.render_widget(
                            Paragraph::new(info).alignment(Alignment::Right),
                            line(layout.status.y),
                        );
                    }
                }
                // A failed `settle` can leave the layout taller than the
                // screen it was drawn into; the cursor is the one thing that
                // cannot be clipped by a rect.
                let row = (top + crow).min(screen.bottom().saturating_sub(1));
                frame.set_cursor_position(Position::new(
                    screen.x + margin + gutter as u16 + ccol,
                    row,
                ));
            })
            .map(|_| ());
        if frame.is_ok() {
            self.dirty = false;
        }
        frame
    }

    /// Paint only when something changed. This is what the loops call: a
    /// setter marks the screen dirty, so no caller has to remember a draw.
    pub fn repaint(&mut self) {
        if self.dirty {
            let _ = self.draw();
        }
    }

    /// The terminal changed under us: the next repaint is not optional. What
    /// `slack` counts is the rows the frame itself gave up, and a blank row
    /// stays blank however the terminal rewraps the rows around it.
    pub fn invalidate(&mut self) {
        self.dirty = true;
    }

    /// Put the frame where the layout says it belongs: `height` rows ending at
    /// the screen bottom. Growing it pushes the last transcript rows into
    /// scrollback first, which is where they belong; shrinking it leaves the
    /// rows it gives up blank above it, which `write` treats as slack.
    fn settle(&mut self, height: u16) -> io::Result<()> {
        let size = self.term.size()?;
        let height = height.min(size.height).max(1);
        let area = self.term.area();
        if area.height == height {
            return Ok(());
        }
        if height > area.height {
            self.term.insert_before(height - area.height, |_| {})?;
        } else {
            // The rows the frame gives up are above the new top, where the new
            // terminal's clear cannot reach.
            self.term.clear()?;
        }
        // An inline area starts at the cursor row and this one's bottom is the
        // screen's, so the cursor goes where the new top has to be.
        let top = size.height - height;
        self.term.set_cursor_position(Position::new(0, top))?;
        self.term.reshape(height)?;
        // Blank the whole region and force a full repaint.
        self.term.clear()?;
        // Count the rows above the new top that are blank now, because they are
        // the transcript's and `write` fills them from the top. The transcript
        // ends one row above the old top less the slack it already had; what
        // moves that end is the frame's own move, plus the scroll a growth needs
        // when the rows do not fit above the frame.
        let mut slack = i32::from(self.slack) + i32::from(top) - i32::from(area.y);
        if height > area.height {
            let scroll = i32::from(area.y) + i32::from(height) - i32::from(size.height);
            slack += scroll.max(0);
        }
        self.slack = slack.clamp(0, i32::from(top)) as u16;
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
        self.ed.insert_char(c);
    }

    /// Text from a paste, newlines and all: it joins the buffer as it is, so a
    /// pasted block is one submission, not one per line. A terminal may hand
    /// over CRLF; that is still one line break.
    pub fn insert_text(&mut self, text: &str) {
        self.dirty = true;
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
        self.ed.insert_text(&text);
    }

    fn fresh(&mut self) -> bool {
        if !self
            .selection
            .as_ref()
            .is_some_and(|sel| sel.belongs(&self.ed.buf))
        {
            let matches = self.candidates();
            self.selection = if matches.is_empty() {
                None
            } else {
                Some(Selection {
                    typed: self.ed.buf.clone(),
                    matches,
                    at: 0,
                })
            };
        }
        self.selection.is_some()
    }

    /// An arrow: move the marker one row, wrapping at both ends. The line is
    /// left exactly as typed — the marker is a choice, not a completion, so
    /// picking one never rewrites what was typed.
    fn mark(&mut self, delta: isize) {
        if !self.fresh() {
            return;
        }
        if let Some(sel) = &mut self.selection {
            let n = sel.matches.len() as isize;
            sel.at = (sel.at as isize + delta).rem_euclid(n) as usize;
        }
    }

    /// Tab: put the marked candidate on the line. When the line already holds
    /// it the marker steps on first, so Tab walks the list; the set was
    /// captured when the list was taken, so the walk covers every line the
    /// prefix matched. A lone candidate finishes with a space and ends the
    /// list.
    fn complete(&mut self) {
        if !self.fresh() {
            return;
        }
        let Some(sel) = &self.selection else { return };
        let n = sel.matches.len();
        let at = if sel.matches[sel.at] == self.ed.buf {
            (sel.at + 1) % n
        } else {
            sel.at
        };
        let lone = n == 1;
        let mut line = sel.matches[at].clone();
        if lone {
            line.push(' ');
        }
        self.ed.buf = line;
        self.ed.cursor = self.ed.len();
        if lone {
            self.selection = None;
        } else if let Some(sel) = &mut self.selection {
            sel.at = at;
            sel.typed = self.ed.buf.clone();
        }
    }

    /// Enter takes what the marker was moved to: a stepped line runs the marked
    /// candidate even though its text is still the prefix it was typed as.
    fn take_marked(&mut self) {
        let Some(sel) = self
            .selection
            .as_ref()
            .filter(|sel| sel.belongs(&self.ed.buf))
        else {
            return;
        };
        if sel.matches[sel.at] == self.ed.buf {
            return;
        }
        let line = sel.matches[sel.at].clone();
        self.ed.buf = line;
        self.ed.cursor = self.ed.len();
    }

    /// The lines the buffer is a prefix of. Only a name is completed while only
    /// a name is typed, so Tab on a bare slash word cannot land on a model; an
    /// argument is completed while it is one word, so `/model dee` matches
    /// `/model deepseek-chat`. A free-text argument takes no candidates, and a
    /// second space ends the matching.
    fn candidates(&self) -> Vec<String> {
        if !self.ed.buf.starts_with('/') {
            return Vec::new();
        }
        match self.ed.buf.split_once(' ') {
            None => self
                .commands
                .iter()
                .filter(|c| !c.contains(' ') && c.starts_with(&self.ed.buf))
                .cloned()
                .collect(),
            Some((_, arg)) if !arg.contains(char::is_whitespace) => self
                .commands
                .iter()
                .filter(|c| c.starts_with(&self.ed.buf))
                .cloned()
                .collect(),
            Some(_) => Vec::new(),
        }
    }

    /// The candidates to list under the prompt and which row the marker is on:
    /// the stepped list, or what is typed now with the first row marked.
    fn hints(&self) -> (Vec<String>, usize) {
        match &self.selection {
            Some(sel) if sel.belongs(&self.ed.buf) => (sel.matches.clone(), sel.at),
            _ => (self.candidates(), 0),
        }
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        self.close();
    }
}

/// Lay the prompt buffer out at `width` cells, with `gutter` cells at the left
/// of every row. Returns the rows (text after the gutter) and the cursor as
/// (row, column after the gutter). A buffer that fills a row to its last cell
/// puts the cursor on the next, empty one: the text broke there, so the cursor
/// did too.
///
/// Every row is returned, not just the ones that fit: how tall the frame is
/// follows the count, and the rows it cannot show are the ones it scrolls past.
fn prompt_layout(
    gutter: usize,
    buf: &str,
    cursor: usize,
    width: usize,
) -> (Vec<String>, (u16, u16)) {
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
    (rows, (at.0 as u16, at.1 as u16))
}

/// The info row as styled pieces joined by `SEP`, cut to `width` cells: the
/// piece that overruns ends with an ellipsis and the rest is dropped, so a
/// narrow row loses its last piece rather than its colors.
fn info_line(parts: &[(Info, String)], width: usize) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut used = 0;
    for (kind, text) in parts {
        if !spans.is_empty() {
            if used + SEP.width() >= width {
                break;
            }
            spans.push(Span::styled(SEP, DIM));
            used += SEP.width();
        }
        let text = clip(text, width - used);
        used += text.width();
        spans.push(Span::styled(text, kind.style()));
    }
    Line::from(spans)
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

/// A row with nothing on it. Blocks do not begin or end with one: the blank row
/// between two blocks is the writer's, not the caller's.
fn blank(line: &Line<'static>) -> bool {
    line.spans.iter().all(|span| span.content.trim().is_empty())
}

/// The rows of a block that go above the frame, at the absolute row `top`.
fn paint_above<B: Backend>(
    backend: &mut B,
    top: u16,
    rows: Vec<Line<'static>>,
    width: u16,
    margin: u16,
) -> io::Result<()> {
    let width = width.max(1);
    let mut buf = Cells::empty(Rect::new(0, top, width, rows.len() as u16));
    paint(&mut buf, rows, margin);
    let cells = buf.content.iter().enumerate().map(|(i, cell)| {
        (
            (i % width as usize) as u16,
            top + (i / width as usize) as u16,
            cell,
        )
    });
    backend.draw(cells)?;
    backend.flush()
}

/// Lay rows out in `buf` with `margin` cells at each side, then mark where each
/// row's text ends.
///
/// The backend prints every cell it is handed, blank ones included, so a row
/// padded to the screen width would leave trailing spaces in scrollback for a
/// later, narrower terminal to reflow into rows of white space. An empty symbol
/// instead marks where the row's text ends: `Tail` draws up to it and erases the
/// rest, which also wipes whatever the row held before.
fn paint(buf: &mut Cells, rows: Vec<Line<'static>>, margin: u16) {
    let width = buf.area.width.max(1) as usize;
    Paragraph::new(rows).render(inset(buf.area, margin), buf);
    for row in buf.content.chunks_mut(width) {
        // Past the last cell that is not blank, and past the columns a wide
        // glyph covers: the mark goes after the character, never inside it.
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
}

/// One row of a buffer as text, without the padding to the screen width: a
/// terminal keeps the text, not the cells that were never written.
#[cfg(test)]
fn row_text(cells: &[Cell]) -> String {
    cells
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>()
        .trim_end()
        .to_string()
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
    // No text is no line at all. Without this, `""` would be one empty line,
    // which is a block boundary: a chunk that carries no newline would end the
    // paragraph or the code fence it arrived in the middle of.
    if text.is_empty() {
        return Vec::new();
    }
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
        // No text is no line, so nothing ends the block being collected.
        assert_eq!(split(""), Vec::<&str>::new());
    }

    #[test]
    fn prompt_layout_wraps_and_tracks_cursor() {
        let (rows, (row, col)) = prompt_layout(2, "ab\ncd", 5, 10);
        assert_eq!(rows, vec!["ab", "cd"]);
        assert_eq!((row, col), (1, 2));
        // A row filled to its last cell breaks the cursor onto the next one.
        let (rows, (row, col)) = prompt_layout(2, "abcdefgh", 8, 10);
        assert_eq!(rows, vec!["abcdefgh", ""]);
        assert_eq!((row, col), (1, 0));
    }

    #[test]
    fn prompt_layout_draws_controls_as_one_cell() {
        let (rows, (row, col)) = prompt_layout(2, "a\u{1}b", 3, 20);
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
    fn repaint_paints_only_what_changed() {
        let mut screen = Screen::test_screen(40, 10);
        screen.draw().unwrap();
        // A frame is out: nothing is pending until a setter runs.
        assert!(!screen.is_dirty());
        screen.set_info([(Info::Selection, "after".to_string())]);
        assert!(screen.is_dirty());
        screen.repaint();
        assert!(!screen.is_dirty());
        let text: String = screen
            .test_buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("after"), "{text}");
        // A transcript line goes to scrollback through `write`, which paints
        // as it goes: nothing is left pending afterwards.
        screen.output("plain text\n");
        assert!(!screen.is_dirty());
    }

    #[test]
    fn live_region_renders_prompt_status_info_and_queue() {
        let mut screen = Screen::test_screen(40, 10);
        screen.ed.buf = "hi".into();
        screen.ed.cursor = 2;
        screen.set_status("Thinking…");
        screen.set_info([
            (Info::Selection, "p/m".to_string()),
            (Info::Level, "default".to_string()),
        ]);
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
        assert!(text.contains("p/m  ·  default"), "{text}");
        assert!(text.contains("Queued: later"), "{text}");
    }

    #[test]
    fn a_long_info_does_not_squeeze_out_the_status() {
        let mut screen = Screen::test_screen(40, 10);
        screen.set_status("Responding…");
        screen.set_info([
            (Info::Title, "a long session title".to_string()),
            (Info::Selection, "mock/mock-1".to_string()),
            (Info::Level, "default".to_string()),
            (Info::Fill(Some(13)), "1125/8192 (13%)".to_string()),
        ]);
        screen.draw().unwrap();
        let text: String = screen
            .test_buffer()
            .content
            .chunks(40)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Responding…"), "{text}");
        // The info keeps its half, clipped, rather than taking the row.
        assert!(text.contains("a long session tit"), "{text}");
    }

    #[test]
    fn every_info_piece_carries_its_own_color() {
        let mut screen = Screen::test_screen(100, 12);
        screen.set_info([
            (Info::Title, "t".to_string()),
            (Info::Selection, "p/m".to_string()),
            (Info::Level, "high".to_string()),
            (Info::Fill(Some(93)), "93%".to_string()),
            (Info::Mode, "read-only".to_string()),
        ]);
        screen.draw().unwrap();
        let layout = screen.frame();
        let cells: Vec<(String, Color)> = (0..100u16)
            .map(|x| {
                let cell = &screen.test_buffer()[(x, layout.status.y)];
                (cell.symbol().to_string(), cell.fg)
            })
            .filter(|(symbol, _)| symbol != " ")
            .collect();
        // Each piece carries one color of its own: the name is the terminal's
        // default, the selection is the prompt's cyan, the level magenta, a
        // nearly full window red, a mode that cannot write yellow.
        let color = |first: &str| {
            let (_, fg) = cells
                .iter()
                .find(|(symbol, _)| symbol == first)
                .unwrap_or_else(|| panic!("{first} is not on the row: {cells:?}"));
            *fg
        };
        assert_eq!(color("t"), Color::Reset);
        assert_eq!(color("p"), Color::Cyan);
        assert_eq!(color("h"), Color::Magenta);
        assert_eq!(color("9"), Color::Red);
        assert_eq!(color("r"), Color::Yellow);
    }

    #[test]
    fn the_prompt_sits_under_the_last_block() {
        let rows = |screen: &Screen| -> Vec<String> {
            screen
                .test_buffer()
                .content
                .chunks(40)
                .map(|row| row.iter().map(|cell| cell.symbol()).collect())
                .collect()
        };
        let mut screen = Screen::test_screen(40, 24);
        // An idle prompt keeps one row above it and no more: the five it needs
        // are that blank row, its own row, a blank, the status and the row
        // under that.
        screen.draw().unwrap();
        assert_eq!(screen.term.area().height, 5);
        screen.echo_user("a question");
        screen.output("an answer");
        screen.draw().unwrap();
        let frame = screen.frame();
        let text = rows(&screen);
        // The last block ends under a blank row, and the prompt is under that:
        // the separation is the frame's, so no block ends with one.
        assert_eq!(text[frame.prompt.y as usize - 2].trim_end(), " an answer");
        assert_eq!(text[frame.prompt.y as usize - 1].trim_end(), "");
        assert_eq!(text[frame.prompt.y as usize].trim_end(), " ❯");
        // A wrapped prompt grows into the frame's own top, and the rows it
        // asks for are the rows it uses; the blank row above it stays.
        screen.insert_text(&"word ".repeat(40));
        screen.draw().unwrap();
        assert_eq!(screen.frame().prompt.height, 3);
        assert_eq!(screen.term.area().height, 7);
    }

    #[test]
    fn an_alert_takes_the_row_the_info_would_use() {
        let mut screen = Screen::test_screen(40, 10);
        screen.set_status("Thinking…");
        screen.set_info([
            (Info::Selection, "p/m".to_string()),
            (Info::Level, "default".to_string()),
        ]);
        screen.set_alert(Some("press ESC again to interrupt"));
        screen.draw().unwrap();
        let text: String = screen
            .test_buffer()
            .content
            .chunks(40)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("press ESC again to interrupt"), "{text}");
        // The question is whole, so the metadata it outranks is not drawn.
        assert!(!text.contains("p/m · default"), "{text}");
        assert!(!text.contains("Thinking…"), "{text}");
    }

    #[test]
    fn editing_keys_do_not_need_a_terminal() {
        let mut screen = Screen::test_screen(40, 10);
        for c in ['a', 'b', 'c'] {
            screen.apply_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        assert_eq!(screen.ed.buf, "abc");
        screen.apply_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        screen.apply_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(screen.ed.buf, "ac");
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
        assert_eq!(screen.ed.buf, "\na");
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
        assert_eq!(screen.ed.buf, "h");
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
        assert_eq!(screen.ed.buf, "/exit");
        screen.apply_key(tab);
        assert_eq!(screen.ed.buf, "/explore");
        screen.apply_key(tab);
        assert_eq!(screen.ed.buf, "/exit");
        // An edit ends the list: the next Tab starts from the text again.
        screen.apply_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE));
        assert_eq!(screen.ed.buf, "/exitp");
        screen.apply_key(tab);
        assert_eq!(screen.ed.buf, "/exitp");
        screen.apply_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        // One candidate left: Tab finishes it and ends the list.
        screen.apply_key(tab);
        assert_eq!(screen.ed.buf, "/exit ");
        assert!(screen.selection.is_none());
    }

    #[test]
    fn tab_finishes_a_lone_candidate_with_a_space() {
        let mut screen = Screen::test_screen(40, 10);
        screen.set_commands(vec!["/help".into(), "/exit".into()]);
        screen.insert_text("/h");
        screen.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(screen.ed.buf, "/help ");
        assert!(screen.selection.is_none());
        // Free text after the first argument is the user's: Tab leaves it
        // alone.
        screen.insert_text("/model");
        screen.insert_text(" a/b");
        screen.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(screen.ed.buf, "/help /model a/b");
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
        assert_eq!(screen.ed.buf, "/model ");
        // Only then does the argument list itself.
        assert_eq!(
            screen.hints(),
            (vec!["/model one/big".into(), "/model two/small".into()], 0)
        );
        screen.apply_key(tab);
        assert_eq!(screen.ed.buf, "/model one/big");
        screen.apply_key(tab);
        assert_eq!(screen.ed.buf, "/model two/small");
        // The typed argument narrows the set; one left finishes the line.
        screen.take();
        screen.insert_text("/model tw");
        screen.apply_key(tab);
        assert_eq!(screen.ed.buf, "/model two/small ");
        assert!(screen.selection.is_none());
        // A second word is free text: no candidates, no fill.
        screen.take();
        screen.insert_text("/model one/big now");
        screen.apply_key(tab);
        assert_eq!(screen.ed.buf, "/model one/big now");
    }

    #[test]
    fn arrows_move_the_marker_and_leave_the_line_alone() {
        let mut screen = Screen::test_screen(40, 10);
        screen.set_commands(vec!["/skill:ship".into(), "/skills".into()]);
        screen.insert_text("/sk");
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        // The list starts on its first row, so the first press moves off it.
        assert_eq!(screen.hints().1, 0);
        screen.apply_key(key(KeyCode::Down));
        assert_eq!(screen.hints().1, 1);
        assert_eq!(screen.ed.buf, "/sk");
        // Both ends wrap.
        screen.apply_key(key(KeyCode::Down));
        assert_eq!(screen.hints().1, 0);
        screen.apply_key(key(KeyCode::Up));
        assert_eq!(screen.hints().1, 1);
        // Up and down agree on where they landed, and Tab takes it.
        screen.apply_key(key(KeyCode::Up));
        screen.apply_key(key(KeyCode::Down));
        assert_eq!(screen.hints().1, 1);
        screen.apply_key(key(KeyCode::Tab));
        assert_eq!(screen.ed.buf, "/skills");
        // Nothing to mark: the arrows leave the line alone.
        screen.take();
        screen.insert_text("hello");
        screen.apply_key(key(KeyCode::Up));
        assert_eq!(screen.ed.buf, "hello");
    }

    #[test]
    fn enter_runs_the_marked_candidate() {
        let mut screen = Screen::test_screen(40, 10);
        screen.set_commands(vec!["/skill:ship".into(), "/skills".into()]);
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        // A list nobody stepped through is a preview: Enter submits the text.
        screen.insert_text("/sk");
        screen.apply_key(key(KeyCode::Enter));
        assert_eq!(screen.take(), "/sk");
        // A marker that was moved makes the line the marked candidate.
        screen.insert_text("/sk");
        screen.apply_key(key(KeyCode::Down));
        assert_eq!(screen.ed.buf, "/sk");
        screen.apply_key(key(KeyCode::Enter));
        assert_eq!(screen.take(), "/skills");
        // An edit ends the list, so Enter is back to what is typed.
        screen.insert_text("/sk");
        screen.apply_key(key(KeyCode::Down));
        screen.apply_key(key(KeyCode::Char('i')));
        assert_eq!(screen.ed.buf, "/ski");
        screen.apply_key(key(KeyCode::Enter));
        assert_eq!(screen.take(), "/ski");
    }

    /// The invariant: one blank row between two blocks, never two, and never a
    /// blank row at either end of the transcript.
    fn packed(screen: &mut Screen) {
        let rows = screen.transcript();
        let blank = |row: &String| row.trim().is_empty();
        for pair in rows.windows(2) {
            assert!(
                !(blank(&pair[0]) && blank(&pair[1])),
                "two blank rows in {rows:?}"
            );
        }
        assert!(rows.first().is_some_and(|row| !blank(row)), "{rows:?}");
        assert!(rows.last().is_some_and(|row| !blank(row)), "{rows:?}");
    }

    #[test]
    fn a_command_and_its_answer_are_one_blank_row_apart() {
        let mut screen = Screen::test_screen(40, 24);
        screen.insert_text("/thinking");
        screen.commit();
        screen.output("thinking: high\n\n");
        assert_eq!(
            screen.transcript(),
            [" █ /thinking", "", " thinking: high"],
            "the frame's rows are below the transcript, not inside it"
        );
        packed(&mut screen);
    }

    #[test]
    fn nothing_a_session_does_leaves_a_blank_row_in_the_transcript() {
        let mut screen = Screen::test_screen(40, 24);
        screen.set_commands(vec![
            "/alpha".into(),
            "/alpha one".into(),
            "/alpha two".into(),
            "/beta".into(),
        ]);
        let key = |c: char| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
        screen.insert_text("hello");
        screen.commit();
        packed(&mut screen);
        screen.output("a reply\n\n");
        packed(&mut screen);
        // The candidate list comes and goes while a command is typed: the frame
        // has the rows for it either way, so the transcript does not move.
        for c in "/alpha".chars() {
            screen.apply_key(key(c));
            screen.repaint();
            packed(&mut screen);
        }
        screen.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        screen.repaint();
        packed(&mut screen);
        screen.commit();
        screen.output("another reply\n");
        packed(&mut screen);
        // A chooser is the other frame and gives its rows back; what it commits
        // to the transcript lands under the block before it.
        screen.set_menu("Sessions", vec!["one".into(), "two".into()], false);
        screen.repaint();
        packed(&mut screen);
        screen.clear_menu(true);
        packed(&mut screen);
        // Resuming from a chooser: the list vanishes without a trace and the
        // session's own transcript replays into the rows it gave back.
        screen.set_menu("Sessions", vec!["one".into(), "two".into()], false);
        screen.repaint();
        screen.clear_menu(false);
        screen.output("Resumed: a title");
        packed(&mut screen);
        screen.echo_user("the old question");
        packed(&mut screen);
        // A resize reflows the terminal and moves the frame; nothing it moves
        // may land between two blocks.
        screen.invalidate();
        screen.repaint();
        packed(&mut screen);
        screen.output("a third reply\n");
        packed(&mut screen);
    }

    #[test]
    fn the_frame_follows_what_it_shows() {
        let mut screen = Screen::test_screen(40, 24);
        let height = screen.term.size().unwrap().height;
        fn check(screen: &mut Screen, want: u16, height: u16) {
            screen.draw().unwrap();
            assert_eq!(screen.term.area().height, want);
            assert_eq!(screen.term.area().top(), height - want);
        }
        // An idle prompt is a blank row, its own row, a blank row, the status
        // and the row under that: nothing is kept for a list that is not there.
        check(&mut screen, 5, height);
        // Candidates take rows while they are shown and give them back after.
        screen.set_commands(vec!["/one".into(), "/two".into(), "/three".into()]);
        screen.insert_text("/");
        check(&mut screen, 7, height);
        screen.take();
        check(&mut screen, 5, height);
        // A prompt that wraps asks for the rows it fills, up to the three it
        // composes in.
        screen.insert_text(&"word ".repeat(40));
        check(&mut screen, 7, height);
        screen.take();
        check(&mut screen, 5, height);
        // A chooser is a window over the transcript, so it is the one part the
        // terminal sizes.
        screen.set_menu("Header", vec!["one".into(), "two".into()], false);
        check(&mut screen, 5 + (height / 3).clamp(2, 10), height);
        screen.clear_menu(false);
        check(&mut screen, 5, height);
    }

    #[test]
    fn hints_are_listed_above_the_prompt() {
        let mut screen = Screen::test_screen(20, 24);
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
        let layout = screen.frame();
        let rows: Vec<String> = screen
            .test_buffer()
            .content
            .chunks(20)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect())
            .collect();
        // One blank cell of margin, then the prompt at the bottom of its box,
        // then the list in the rows above it: a list that appears moves
        // nothing.
        assert_eq!(
            rows[layout.prompt.bottom() as usize - 1].trim_end(),
            " ❯ /sk"
        );
        assert_eq!(rows[layout.list.y as usize].trim_end(), "   /skills");
        assert_eq!(
            rows[layout.list.y as usize + 1].trim_end(),
            "   /skill:ship"
        );
        // The marked row is the bright one.
        let cell = |x: u16, y: u16| screen.test_buffer()[(x, y)].clone();
        assert_eq!(cell(3, layout.list.y).fg, Color::Cyan);
        assert_ne!(cell(3, layout.list.y + 1).fg, Color::Cyan);
        // After a step the whole set stays listed, with the buffer on the
        // second one.
        screen.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(screen.ed.buf, "/skills");
        assert_eq!(
            screen.hints(),
            (vec!["/skills".into(), "/skill:ship".into()], 0)
        );
        screen.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(screen.ed.buf, "/skill:ship");
        assert_eq!(screen.hints().1, 1);
    }

    #[test]
    fn short_terminal_lists_fewer_candidates() {
        let mut screen = Screen::test_screen(20, 14);
        screen.set_commands(vec!["/c1".into(), "/c2".into(), "/c3".into()]);
        screen.insert_text("/c");
        screen.draw().unwrap();
        let layout = screen.frame();
        let rows: Vec<String> = screen
            .test_buffer()
            .content
            .chunks(20)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect())
            .collect();
        // A short terminal keeps rows for the transcript: the list is capped,
        // and the candidates past it wait for the selection to walk down.
        assert_eq!(layout.list.height, 1);
        assert_eq!(rows[layout.list.y as usize].trim_end(), "   /c1");
        // One blank row, then the prompt: the row above it is the prompt's own
        // and the list does not take it back.
        assert_eq!(rows[layout.prompt.y as usize - 1].trim_end(), "");
        assert_eq!(rows[layout.prompt.y as usize].trim_end(), " ❯ /c");
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
        assert_eq!(screen.ed.buf, "/c7");
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
        // The screen above the frame is gone, so the frame stands at the bottom
        // of the terminal again, not where the last block walked it to.
        let size = screen.term.size().unwrap();
        let area = screen.term.area();
        assert_eq!(area.top(), size.height - area.height);
        assert!(screen.menu.is_none());
        assert!(screen.status().is_empty());
        assert!(screen.last.is_none());
        // Nothing is left to lay out: a flush finds an empty stream and writes
        // no block, so the new session's first text has no separator to skip.
        screen.flush();
        assert!(screen.last.is_none());
    }

    #[test]
    fn the_transcript_is_everything_that_scrolled_off() {
        let mut screen = Screen::test_screen(40, 10);
        for i in 0..12 {
            screen.output(&format!("line {i}\n"));
        }
        let transcript = screen.transcript();
        // The screen holds ten rows minus the live region; the rest is history.
        // A row keeps the one cell of margin it was written with.
        assert_eq!(transcript.last().unwrap(), " line 11", "{transcript:?}");
        assert!(
            transcript.iter().any(|row| row == " line 0"),
            "{transcript:?}"
        );
    }

    #[test]
    fn a_thought_arriving_in_chunks_lays_out_like_a_whole_one() {
        let text = "Let me check:\n\n```rust\nlet x = 1;\nlet y = 2;\n```\n\ndone\n";
        // Resume hands the screen the whole thought at once.
        let mut whole = Screen::test_screen(40, 24);
        whole.thought(text);
        whole.flush();
        // A turn hands it one delta at a time, and only the finished lines: a
        // delta with no newline leaves nothing to pass on.
        let mut streamed = Screen::test_screen(40, 24);
        let mut pending = String::new();
        for delta in text.as_bytes().chunks(3) {
            pending.push_str(std::str::from_utf8(delta).unwrap());
            let finished = match pending.rfind('\n') {
                Some(at) => pending.drain(..=at).collect::<String>(),
                None => String::new(),
            };
            streamed.thought(&finished);
        }
        streamed.thought(&pending);
        streamed.flush();
        assert_eq!(streamed.transcript(), whole.transcript());
        // The code is one block: no blank row inside it.
        let rows = streamed.transcript();
        assert!(
            rows.windows(2)
                .all(|pair| !(pair[0].is_empty() && pair[1].is_empty())),
            "{rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("let y = 2;")),
            "{rows:?}"
        );
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
