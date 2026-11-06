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
        let end = self.cursor;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A line typed in, with the cursor at its end.
    fn typed(line: &str) -> Buffer {
        let mut buffer = Buffer::default();
        buffer.insert_text(line);
        buffer
    }

    #[test]
    fn transpose_swaps_the_two_chars_before_the_cursor() {
        let mut buffer = typed("abc");
        buffer.transpose_chars();
        assert_eq!(buffer.buf, "acb");
        assert_eq!(buffer.cursor, 3);
        // Nothing before the cursor to swap.
        let mut buffer = typed("a");
        buffer.transpose_chars();
        assert_eq!(buffer.buf, "a");
    }

    #[test]
    fn transpose_counts_chars_not_bytes() {
        // `é` is two bytes: a byte offset here stepped past the buffer and
        // `char_at` returned nothing to the unwrap.
        let mut buffer = typed("café");
        buffer.transpose_chars();
        assert_eq!(buffer.buf, "caéf");
        assert_eq!(buffer.cursor, 4);
        // One char, no swap, no panic.
        let mut buffer = typed("é");
        buffer.transpose_chars();
        assert_eq!(buffer.buf, "é");
        // Mid-line, past a wide char.
        let mut buffer = typed("日本x");
        buffer.cursor = 2;
        buffer.transpose_chars();
        assert_eq!(buffer.buf, "本日x");
        assert_eq!(buffer.cursor, 2);
    }

    #[test]
    fn the_cursor_can_be_sent_home_and_to_the_end() {
        let mut buffer = typed("one two");
        buffer.cursor = 4;
        buffer.word_back();
        assert_eq!(buffer.cursor, 0, "the space and the word before it");
        buffer.word_back();
        assert_eq!(buffer.cursor, 0, "the line has a start");
        buffer.word_forward();
        assert_eq!(buffer.cursor, 3);
        buffer.word_forward();
        assert_eq!(buffer.cursor, 7, "past the last word is the end");
        buffer.word_forward();
        assert_eq!(buffer.cursor, 7);

        // From inside a word the cursor goes to that word's start.
        buffer.cursor = 6;
        buffer.word_back();
        assert_eq!(buffer.cursor, 4);
        // Whitespace before the cursor is stepped over with the word.
        let mut buffer = typed("one   two");
        buffer.cursor = 6;
        buffer.word_back();
        assert_eq!(buffer.cursor, 0);
    }

    #[test]
    fn a_word_kill_takes_the_words_and_remembers_them() {
        let mut buffer = typed("alpha beta gamma");
        buffer.kill_word_back();
        assert_eq!(buffer.buf, "alpha beta ");
        assert_eq!(buffer.cursor, 11);
        assert_eq!(buffer.killed, "gamma");
        buffer.kill_word_forward();
        assert_eq!(buffer.buf, "alpha beta ", "nothing ahead to kill");

        let mut buffer = typed("alpha beta");
        buffer.cursor = 2;
        buffer.kill_word_forward();
        assert_eq!(buffer.buf, "al beta", "from the cursor to the word's end");
        assert_eq!(buffer.cursor, 2);
        assert_eq!(buffer.killed, "pha");
    }

    #[test]
    fn a_kill_to_the_edges_takes_the_rest_of_the_line() {
        let mut buffer = typed("one two");
        buffer.cursor = 3;
        buffer.kill_to_start();
        assert_eq!(buffer.buf, " two");
        assert_eq!(buffer.cursor, 0);
        assert_eq!(buffer.killed, "one");
        buffer.kill_to_end();
        assert_eq!(buffer.buf, "");
        assert_eq!(buffer.killed, " two");

        // At the edges there is nothing to kill, and what was killed stays.
        let mut buffer = typed("abc");
        buffer.killed = "kept".into();
        buffer.cursor = 0;
        buffer.kill_to_start();
        assert_eq!(buffer.buf, "abc");
        assert_eq!(buffer.killed, "kept");
        buffer.cursor = 3;
        buffer.kill_to_end();
        assert_eq!(buffer.buf, "abc");
        assert_eq!(buffer.killed, "kept");
    }

    #[test]
    fn yank_puts_the_killed_text_at_the_cursor() {
        let mut buffer = typed("one two");
        buffer.kill_word_back();
        assert_eq!(buffer.buf, "one ");
        buffer.yank();
        assert_eq!(buffer.buf, "one two");
        assert_eq!(buffer.cursor, 7);
        buffer.cursor = 0;
        buffer.yank();
        assert_eq!(buffer.buf, "twoone two");
        assert_eq!(buffer.cursor, 3);

        // Nothing killed yet: a yank is not an empty insertion.
        let mut buffer = typed("abc");
        buffer.cursor = 1;
        buffer.yank();
        assert_eq!(buffer.buf, "abc");
        assert_eq!(buffer.cursor, 1);
    }

    #[test]
    fn backspace_and_delete_stop_at_the_edges() {
        let mut buffer = typed("abc");
        buffer.cursor = 1;
        buffer.backspace();
        assert_eq!(buffer.buf, "bc");
        assert_eq!(buffer.cursor, 0);
        buffer.backspace();
        assert_eq!(buffer.buf, "bc", "nothing before the start");
        buffer.delete();
        assert_eq!(buffer.buf, "c");
        buffer.delete();
        assert_eq!(buffer.buf, "");
        buffer.delete();
        assert_eq!(buffer.buf, "", "nothing past the end");

        // Wide chars are one step each.
        let mut buffer = typed("日本");
        buffer.cursor = 1;
        buffer.backspace();
        assert_eq!(buffer.buf, "本");
        assert_eq!(buffer.cursor, 0);
    }

    #[test]
    fn transpose_words_swaps_the_pair_before_the_cursor() {
        let mut buffer = typed("alpha beta");
        buffer.transpose_words();
        assert_eq!(buffer.buf, "beta alpha");
        assert_eq!(buffer.cursor, 10);

        // Trailing whitespace is stepped over, not swapped.
        let mut buffer = typed("alpha beta   ");
        buffer.transpose_words();
        assert_eq!(buffer.buf, "beta alpha   ");

        // The space between the two words travels with them.
        let mut buffer = typed("one  two");
        buffer.transpose_words();
        assert_eq!(buffer.buf, "two  one");

        // One word, or none, is nothing to swap.
        let mut buffer = typed("only");
        buffer.transpose_words();
        assert_eq!(buffer.buf, "only");
        assert_eq!(buffer.cursor, 4);
        let mut buffer = typed("   ");
        buffer.transpose_words();
        assert_eq!(buffer.buf, "   ");
        let mut buffer = typed("one two");
        buffer.cursor = 3;
        buffer.transpose_words();
        assert_eq!(
            buffer.buf, "one two",
            "two words are needed before the cursor"
        );
    }

    #[test]
    fn a_paste_joins_the_line_where_the_cursor_is() {
        let mut buffer = typed("ac");
        buffer.cursor = 1;
        buffer.insert_text("b\nline");
        assert_eq!(buffer.buf, "ab\nlinec");
        assert_eq!(buffer.cursor, 7);
        buffer.insert_char('!');
        assert_eq!(buffer.buf, "ab\nline!c");
        assert_eq!(buffer.cursor, 8);
        assert_eq!(buffer.take(), "ab\nline!c");
        assert!(buffer.buf.is_empty());
        assert_eq!(buffer.cursor, 0);
        assert_eq!(buffer.offset(0), 0);
        assert_eq!(
            buffer.offset(99),
            0,
            "past the end is the end of an empty line"
        );
    }
}
