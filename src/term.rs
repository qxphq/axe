//! The prompt editor: it owns the line, not the input.
//!
//! Raw mode for the whole session (`RawTerminal`'s `Drop` restores termios), so
//! every key is ours and the kernel echoes nothing. The line is drawn and
//! redrawn in place after each event: `drawn` remembers how many rows the last
//! drawing covered and where the cursor sat in it, which is all that is needed
//! to erase it without scrolling.
//!
//! Wrapping is the terminal's (no newlines inside a drawing); the row/column
//! math below mirrors it: the buffer is one stream of cells that breaks every
//! `cols` columns. A full last row leaves the cursor pending-wrap on the row
//! above, which is why the bottom case clamps instead of stepping down.
//!
//! This type never reads stdin. `apply_key` mutates, `draw` renders, and both
//! are driven by the caller's event loop, so drawing can be interleaved with
//! streamed model text while the user types.

use std::io::{self, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};

use termion::event::Key;
use termion::raw::{IntoRawMode, RawTerminal};
use termion::terminal_size;

/// What a key asked the caller to do.
pub enum Action {
    /// Redraw and keep going.
    None,
    /// The line is complete; the caller takes it with `take`.
    Submit,
    /// End of input.
    Eof,
}

pub struct Editor {
    /// Raw mode for the session. Termios is restored when this drops, so the
    /// field is a guard: output still goes to `io::stdout()` like everywhere
    /// else, since raw mode is a property of the tty, not of the handle.
    raw: Option<RawTerminal<io::Stdout>>,
    /// Plain text: the editor owns the color, so callers pass none.
    prompt: String,
    prompt_width: usize,
    buf: String,
    /// Cursor position in `buf`, in chars.
    cursor: usize,
    cols: usize,
    /// (rows, cursor row) of the last drawing.
    drawn: Option<(usize, usize)>,
    /// One dim line under the prompt: transient state (thinking, the line
    /// being streamed).
    status: String,
    /// The line submitted mid-turn, waiting for the boundary. Its own row: the
    /// status row changes constantly, and a queued line that scrolls away
    /// silently looks like the Enter was lost.
    queued: Option<String>,
}

impl Editor {
    pub fn new() -> Editor {
        Editor {
            raw: None,
            prompt: "> ".to_string(),
            prompt_width: 2,
            buf: String::new(),
            cursor: 0,
            cols: 80,
            drawn: None,
            status: String::new(),
            queued: None,
        }
    }

    /// Change the prompt for the next line. The picker asks for a different
    /// one without tearing down the terminal.
    pub fn prompt(&self) -> &str {
        &self.prompt
    }

    pub fn set_prompt(&mut self, prompt: &str) {
        self.prompt = prompt.to_string();
        self.prompt_width = prompt.chars().count();
    }

    /// Take the terminal. Fails soft: without a tty the editor stays cooked and
    /// the terminal does the line editing.
    pub fn open(mut self) -> Editor {
        if io::stdin().is_terminal() && io::stdout().is_terminal() {
            self.raw = io::stdout().into_raw_mode().ok();
            // `out` translates newlines only while this is set: the kernel is
            // doing it for us until raw mode is actually taken.
            if self.raw.is_some() {
                RAW.store(true, Ordering::SeqCst);
            }
        }
        self
    }

    /// Fold one key into the buffer. Holds no interrupt semantics: ESC belongs
    /// to the caller, which knows whether a turn is in flight.
    pub fn apply_key(&mut self, key: Key) -> Action {
        match key {
            // ENTER arrives as a newline; the editor keeps no newlines.
            Key::Char('\n') => return Action::Submit,
            Key::Char(c) if !c.is_control() => self.insert(c),
            Key::Backspace => self.backspace(),
            Key::Delete => self.delete(),
            Key::Left => self.cursor = self.cursor.saturating_sub(1),
            Key::Right => self.cursor = (self.cursor + 1).min(self.len()),
            Key::Home => self.cursor = 0,
            Key::End => self.cursor = self.len(),
            // A shell's Ctrl-C: drop what was typed, keep the prompt.
            Key::Ctrl('c') => {
                self.buf.clear();
                self.cursor = 0;
            }
            Key::Ctrl('d') if self.buf.is_empty() => return Action::Eof,
            // Arrows up/down have no history yet, and a lone ESC is not input.
            _ => {}
        }
        Action::None
    }

    /// The completed line, leaving the buffer empty.
    pub fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.buf)
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Set the dim line under the prompt. Empty clears it.
    pub fn set_status(&mut self, text: &str) {
        self.status = text.to_string();
    }

    pub fn status(&self) -> &str {
        &self.status
    }

    /// Show, or clear with `None`, the line waiting for the turn boundary.
    pub fn set_queued(&mut self, line: Option<&str>) {
        self.queued = line.map(str::to_string);
    }

    /// Finish the line: erase the prompt drawing and echo the line in the
    /// prompt color. Returns what was committed.
    pub fn commit(&mut self) -> String {
        let line = self.take();
        self.erase();
        let text = format!("{}{}", self.prompt, line);
        let text = self.styled(&text);
        out(&format!("{text}\n"));
        line
    }

    /// User text in the prompt's color, like the shell's.
    fn styled(&self, text: &str) -> String {
        if self.raw.is_some() {
            format!("\x1b[36m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }

    /// Remove the last drawing, leaving the cursor where it started.
    pub fn erase(&mut self) {
        let Some((_, row)) = self.drawn.take() else {
            return;
        };
        let mut s = String::from("\r");
        if row > 0 {
            s.push_str(&format!("\x1b[{row}A"));
        }
        s.push_str("\x1b[0J");
        out(&s);
    }

    /// Replace the last drawing with the current state, cursor in the buffer.
    pub fn draw(&mut self) {
        // Termion reports no resize event, so the width is re-read here. One
        // ioctl against output that is already ioctl-heavy. A pty with no
        // window size reports 0, which would collapse the math to one column.
        self.cols = terminal_size()
            .map(|(w, _)| w as usize)
            .ok()
            .filter(|w| *w > 0)
            .unwrap_or(80);

        let (text, rows, row, col) = self.layout();
        let mut s = String::new();
        if let Some((_, up)) = self.drawn {
            s.push('\r');
            if up > 0 {
                s.push_str(&format!("\x1b[{up}A"));
            }
            s.push_str("\x1b[0J");
        }
        s.push_str(&text);
        // The cursor sits in the prompt row; the status line hangs below it.
        if rows - 1 > row {
            s.push_str(&format!("\x1b[{}A", rows - 1 - row));
        }
        s.push('\r');
        if col > 0 {
            s.push_str(&format!("\x1b[{col}C"));
        }
        out(&s);
        self.drawn = Some((rows, row));
    }

    /// Prompt, buffer and status as one string, plus the geometry the cursor
    /// needs. The status line is part of the same drawing so that erasing it is
    /// the same operation as erasing the prompt.
    fn layout(&self) -> (String, usize, usize, usize) {
        let mut text = self.styled(&self.prompt);
        text.push_str(&self.buf);

        let cols = self.cols.max(1);
        let rows = (self.prompt_width + self.len()).div_ceil(cols).max(1);
        let pos = self.prompt_width + self.cursor;
        let mut row = pos / cols;
        let mut col = pos % cols;
        if row >= rows {
            row = rows - 1;
            col = cols - 1;
        }

        if !self.status.is_empty() {
            text.push('\n');
            // Dim, and truncated to the line so a long message cannot wrap.
            text.push_str("\x1b[2m");
            text.extend(self.status.chars().take(cols));
            text.push_str("\x1b[0m");
        }
        if let Some(queued) = &self.queued {
            text.push('\n');
            text.push_str("\x1b[2mqueued: ");
            text.extend(queued.chars().take(cols.saturating_sub(8)));
            text.push_str("\x1b[0m");
        }
        (text, rows, row, col)
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

    fn insert(&mut self, c: char) {
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

/// Read one line with cooked input. Used when stdin or stdout is not a tty:
/// there is no terminal to own, so the kernel does the editing.
pub fn cooked_line(prompt: &str) -> Option<String> {
    print!("{prompt}");
    io::stdout().flush().ok();
    let mut line = String::new();
    // Qualified: termion's `TermRead` also has a `read_line`, and it would win.
    if io::BufRead::read_line(&mut io::stdin().lock(), &mut line).ok()? == 0 {
        return None;
    }
    Some(line.trim_end_matches(['\n', '\r']).to_string())
}

/// Raw mode clears `OPOST`, so `ONLCR` is gone and a bare `\n` no longer
/// returns the carriage. Everything that prints while the editor owns the
/// terminal therefore goes through here, which is what the terminal would
/// otherwise have done for us.
pub fn out(text: &str) {
    if !raw() {
        print!("{text}");
        io::stdout().flush().ok();
        return;
    }
    let mut stdout = io::stdout();
    stdout.write_all(translated(text).as_bytes()).ok();
    stdout.flush().ok();
}

/// The same for stderr, which shares the tty and therefore the same mode.
pub fn err(text: &str) {
    if !raw() {
        eprint!("{text}");
        return;
    }
    eprint!("{}", translated(text));
}

/// `\n` -> `\r\n`. Raw mode clears `OPOST`, so the kernel no longer does it.
fn translated(text: &str) -> String {
    let mut buf = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch == '\n' {
            buf.push('\r');
        }
        buf.push(ch);
    }
    buf
}

/// True once raw mode is ours.
pub fn raw() -> bool {
    RAW.load(Ordering::SeqCst)
}

static RAW: AtomicBool = AtomicBool::new(false);

/// True when the editor can own the terminal.
pub fn is_interactive() -> bool {
    io::stdin().is_terminal() && io::stdout().is_terminal()
}
