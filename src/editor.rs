//! The prompt's line editor: a buffer, a char cursor, and the keys readline
//! taught everyone. It knows about no screen, no chooser and no turn — the
//! caller folds a key into it and reads the line back.

/// The text being typed. `cursor` is a char index, not a byte offset: the
/// editors' motions are per character, and `offset` converts at the edges.
#[derive(Default)]
pub struct Buffer {
    pub(crate) buf: String,
    pub(crate) cursor: usize,
    /// The last killed text: one slot, enough for Ctrl-U ... Ctrl-Y.
    pub(crate) killed: String,
}

impl Buffer {
    /// The completed line, leaving the buffer empty.
    pub(crate) fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.buf)
    }

    pub(crate) fn len(&self) -> usize {
        self.buf.chars().count()
    }

    /// Byte offset of a char index.
    pub(crate) fn offset(&self, chars: usize) -> usize {
        self.buf
            .char_indices()
            .nth(chars)
            .map(|(byte, _)| byte)
            .unwrap_or(self.buf.len())
    }

    pub(crate) fn insert_char(&mut self, c: char) {
        let at = self.offset(self.cursor);
        self.buf.insert(at, c);
        self.cursor += 1;
    }

    /// Text from a paste, newlines and all: it joins the buffer as it is, so a
    /// pasted block is one submission, not one per line.
    pub(crate) fn insert_text(&mut self, text: &str) {
        let at = self.offset(self.cursor);
        self.buf.insert_str(at, text);
        self.cursor += text.chars().count();
    }

    /// The list under the prompt, taken from the line as it is now with the
    /// first row marked, or the one a step already made. False when nothing
    /// matches.
    pub(crate) fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            let at = self.offset(self.cursor);
            self.buf.remove(at);
        }
    }

    pub(crate) fn delete(&mut self) {
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

    pub(crate) fn word_back(&mut self) {
        self.cursor = self.word_start();
    }

    pub(crate) fn word_forward(&mut self) {
        self.cursor = self.word_end();
    }

    pub(crate) fn kill_word_back(&mut self) {
        let start = self.word_start();
        if start < self.cursor {
            self.killed = self.buf[self.offset(start)..self.offset(self.cursor)].to_string();
            self.splice(start, self.cursor, "");
        }
    }

    pub(crate) fn kill_word_forward(&mut self) {
        let end = self.word_end();
        if end > self.cursor {
            self.killed = self.buf[self.offset(self.cursor)..self.offset(end)].to_string();
            self.splice(self.cursor, end, "");
        }
    }

    pub(crate) fn kill_to_start(&mut self) {
        if self.cursor > 0 {
            self.killed = self.buf[..self.offset(self.cursor)].to_string();
            self.splice(0, self.cursor, "");
        }
    }

    pub(crate) fn kill_to_end(&mut self) {
        if self.cursor < self.len() {
            self.killed = self.buf[self.offset(self.cursor)..].to_string();
            self.splice(self.cursor, self.len(), "");
        }
    }

    pub(crate) fn yank(&mut self) {
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
    pub(crate) fn transpose_chars(&mut self) {
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
    pub(crate) fn transpose_words(&mut self) {
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
