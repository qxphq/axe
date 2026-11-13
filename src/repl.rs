//! The terminal's line loop: the input thread, the prompt's keys, the
//! chooser, and the queue the frame draws while a turn runs. What a line means
//! is the session's business; this module produces lines and paints the frame.

use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::http;
use crate::render::Paint;
use crate::turn::Queue;
use crate::ui;

/// What the prompt yielded: a line, or a mode toggle that is not one.
pub(crate) enum Input {
    /// The committed line.
    Line(String),
    /// Shift+Tab: explore mode, handled without touching the history.
    Explore,
    /// Enter on a chooser: the selected row.
    Menu(usize),
    /// Esc on a chooser.
    Cancel,
}

/// Events from the two producers that never draw: keys, and the turn.
pub(crate) enum Event {
    Key(ratatui::crossterm::event::KeyEvent),
    /// Bracketed paste: inserted at the cursor, never submitted on its own.
    Paste(String),
    /// Transcript text: finished model lines, already spaced.
    Printed(String),
    /// One tool call and its outcome, as styled spans, and the failure's own
    /// words when it failed.
    Echo(Vec<(Paint, String)>, Option<String>),
    /// A transient label for the status row.
    Status(String),
    /// The round is not a whole answer: a provider cut or a stream that died.
    /// The harness's own line, under the text it is about.
    Cut(String),
    /// Streamed reasoning, before any answer text.
    Thought(String),
    /// A line typed during the turn that the turn has just taken into its
    /// history: the transcript shows it where the model saw it.
    User(String),
    /// The terminal changed size; the live region must repaint.
    Resize,
    Done(Result<Option<u64>, String>),
    Eof,
}

/// How long a held `[` waits for the rest of a cursor report: a terminal
/// answers in one burst, a person does not type that fast.
const REPORT_WINDOW: Duration = Duration::from_millis(50);

/// A report is short (`[24;80R`); a longer run is typing that happens to look
/// like one at its start.
const REPORT_MAX: usize = 12;

/// A cursor-position report whose escape was read as a key: ratatui's inline
/// viewport asks the terminal where the cursor is (at startup, and on every
/// resize), and the answer — `ESC [ row ; col R` — arrives on stdin. When it
/// splits, the escape is a key press and the rest reads as typing. A run that
/// starts with `[` and reads as `[0-9;]*R`, in one burst, is a report and is
/// dropped; anything else is handed on unchanged.
#[derive(Default)]
struct CursorReport {
    held: Vec<KeyEvent>,
    started: Option<Instant>,
}

impl CursorReport {
    /// The keys to forward. Empty while a report is being swallowed.
    fn feed(&mut self, key: KeyEvent) -> Vec<KeyEvent> {
        if self.held.is_empty() {
            if plain(key, '[') {
                self.hold(key);
                return Vec::new();
            }
            return vec![key];
        }
        if self.expired() {
            return self.flush(Some(key));
        }
        match key.code {
            KeyCode::Char('R') if plain(key, 'R') => {
                self.held.clear();
                self.started = None;
                Vec::new()
            }
            KeyCode::Char(c)
                if plain(key, c)
                    && (c.is_ascii_digit() || c == ';')
                    && self.held.len() < REPORT_MAX =>
            {
                self.hold(key);
                Vec::new()
            }
            _ => self.flush(Some(key)),
        }
    }

    /// A held `[` that never became a report was typing: hand it over.
    fn flush_expired(&mut self) -> Vec<KeyEvent> {
        if self.expired() {
            return self.flush(None);
        }
        Vec::new()
    }

    fn hold(&mut self, key: KeyEvent) {
        if self.held.is_empty() {
            self.started = Some(Instant::now());
        }
        self.held.push(key);
    }

    fn flush(&mut self, key: Option<KeyEvent>) -> Vec<KeyEvent> {
        self.started = None;
        let mut keys = std::mem::take(&mut self.held);
        keys.extend(key);
        keys
    }

    fn expired(&self) -> bool {
        self.started.is_some_and(|at| at.elapsed() > REPORT_WINDOW)
    }
}

/// Whether the key types `c` with no control or alt chord. Shift is not a
/// chord: the report's `R` is uppercase, and a terminal reports it as shifted.
fn plain(key: KeyEvent, c: char) -> bool {
    key.code == KeyCode::Char(c)
        && !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
}

/// The terminal's keys. Nothing else may read stdin.
pub(crate) fn spawn_input(tx: Sender<Event>) {
    thread::spawn(move || {
        use ratatui::crossterm::event::{poll, read, Event as TermEvent};
        let mut report = CursorReport::default();
        loop {
            for key in report.flush_expired() {
                if tx.send(Event::Key(key)).is_err() {
                    return;
                }
            }
            // A zero timeout, not `read`: waiting for a key holds crossterm's
            // event-reader lock, and the screen needs it to ask the terminal
            // where the cursor is when the prompt wraps. A poll holds it for
            // microseconds and releases it; input is picked up a few
            // milliseconds later.
            match poll(Duration::ZERO) {
                Ok(true) => {}
                Ok(false) => {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(_) => break,
            }
            match read() {
                // Key releases (kitty protocol) are not new input.
                Ok(TermEvent::Key(key)) if key.kind == KeyEventKind::Press => {
                    for key in report.feed(key) {
                        if tx.send(Event::Key(key)).is_err() {
                            return;
                        }
                    }
                }
                Ok(TermEvent::Paste(text)) if !text.is_empty() => {
                    if tx.send(Event::Paste(text)).is_err() {
                        return;
                    }
                }
                Ok(TermEvent::Resize(_, _)) => {
                    if tx.send(Event::Resize).is_err() {
                        return;
                    }
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        let _ = tx.send(Event::Eof);
    });
}

/// A line submitted mid-turn, waiting for the round boundary that lets it join
/// the turn. `turn` says the model will read it, which is what Up can take
/// back; a command waits for the turn's end instead and never enters the turn's
/// own queue.
pub(crate) struct Pending {
    pub(crate) line: String,
    pub(crate) turn: bool,
}

/// The queue as the frame draws it: the waiting lines, oldest first.
pub(crate) fn show_queue(screen: &mut ui::Screen, queue: &[Pending]) {
    let lines: Vec<String> = queue.iter().map(|entry| entry.line.clone()).collect();
    screen.set_queued(&lines);
}

/// What Up takes back: the command lines, which never went to the turn, and the
/// lines the turn has not drained yet — draining its queue here is what makes
/// them unsent. The rest were read, and only their echo is borrowed: the count
/// comes back for the caller to swallow.
pub(crate) fn take_back(queue: &Queue, queued: &[Pending]) -> (Vec<String>, usize) {
    let unsent = queue
        .lock()
        .map(|mut lines| lines.drain(..).count())
        .unwrap_or(0);
    let waiting = queued.iter().filter(|entry| entry.turn).count();
    let read = waiting.saturating_sub(unsent);
    let mut kept = Vec::new();
    let mut seen = 0;
    for entry in queued {
        if !entry.turn {
            kept.push(entry.line.clone());
            continue;
        }
        seen += 1;
        if seen > read {
            kept.push(entry.line.clone());
        }
    }
    (kept, read)
}

/// Put waiting lines back on the prompt, above whatever was typed: they came
/// first, so the turn would have read them first.
pub(crate) fn recall_lines(screen: &mut ui::Screen, lines: &[String]) {
    if lines.is_empty() {
        return;
    }
    let mut text = lines.join("\n");
    if !screen.is_empty() {
        let typed = screen.take();
        text.push('\n');
        text.push_str(&typed);
    }
    screen.insert_text(&text);
}

/// How often the idle prompt looks for work that finished off the UI thread.
pub(crate) const IDLE: Duration = Duration::from_millis(100);

/// Wait for work another thread is doing while the prompt stays live: keys are
/// folded into the line, Esc asks the cancel for it, and the loop ends when the
/// work is finished. What the thread joined to comes back with the one line
/// typed while it ran — a panic is `None`, and the caller names it.
///
/// `!cmd` and `/compact` wait the same way, which is why this is one function:
/// the two used to be one loop each, and only one of them took a line.
pub(crate) fn pump<T>(
    screen: &mut ui::Screen,
    rx: &Receiver<Event>,
    cancel: &http::Cancel,
    handle: thread::ScopedJoinHandle<'_, T>,
) -> (Option<T>, Option<String>) {
    let mut queued: Option<String> = None;
    loop {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(Event::Key(key)) if key.code == KeyCode::Esc => {
                cancel.cancel();
                screen.set_status("× Interrupting");
                screen.repaint();
            }
            Ok(Event::Key(key)) => match screen.apply_key(key) {
                ui::Action::Submit if !screen.is_empty() && queued.is_none() => {
                    queued = Some(screen.take());
                }
                ui::Action::Eof => {
                    cancel.cancel();
                    break;
                }
                _ => {}
            },
            Ok(Event::Paste(text)) => screen.insert_text(&text),
            Ok(Event::Resize) => screen.invalidate(),
            Ok(Event::Eof) => {
                cancel.cancel();
                break;
            }
            Ok(_) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if handle.is_finished() {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                cancel.cancel();
                break;
            }
        }
        // A setter marked the screen; one paint per event.
        screen.repaint();
    }
    (handle.join().ok(), queued)
}

/// Collect one line from the prompt. Keys are the only input while idle;
/// `idle` runs on every tick, so work that finishes on another thread — the
/// session's title — lands without waiting for a key.
pub(crate) fn readline(
    screen: &mut ui::Screen,
    rx: &Receiver<Event>,
    idle: &mut dyn FnMut(&mut ui::Screen),
) -> Option<Input> {
    screen.repaint();
    loop {
        let event = match rx.recv_timeout(IDLE) {
            Ok(event) => event,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                idle(screen);
                screen.repaint();
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return None,
        };
        match event {
            Event::Key(key) => match screen.apply_key(key) {
                ui::Action::Submit if screen.is_empty() => {}
                ui::Action::Submit => return Some(Input::Line(screen.commit())),
                ui::Action::Explore => return Some(Input::Explore),
                ui::Action::MenuStep(delta) => {
                    screen.menu_move(delta);
                    screen.repaint();
                }
                ui::Action::MenuAccept => return Some(Input::Menu(screen.menu_selected())),
                ui::Action::MenuCancel => {
                    // The list stays in the transcript: a chooser that closes
                    // must not leave blank rows where it was.
                    screen.clear_menu(true);
                    screen.repaint();
                    return Some(Input::Cancel);
                }
                ui::Action::Eof => return None,
                // A queue belongs to a turn, and none is running here.
                ui::Action::Recall => {}
                ui::Action::None => {
                    screen.repaint();
                }
            },
            Event::Paste(text) => {
                screen.insert_text(&text);
                screen.repaint();
            }
            Event::Resize => {
                screen.invalidate();
                screen.repaint();
            }
            Event::Eof => return None,
            // A turn's events cannot arrive while idle; drop them if they do.
            _ => {}
        }
    }
}

/// The items a query keeps: a case-insensitive substring of the key, which is
/// what typing matches, never the row with its context window.
pub(crate) fn narrow(items: &[(String, String)], query: &str) -> Vec<(String, String)> {
    let query = query.to_lowercase();
    items
        .iter()
        .filter(|(key, _)| key.to_lowercase().contains(&query))
        .cloned()
        .collect()
}

/// Walk a chooser until Enter, Esc or the end of input: the arrows move, a
/// typed line narrows the list, Enter takes a row. `find` answers a query with
/// the rows to offer, as (key, row) — the key is what Enter is worth, the row
/// what the screen draws — and `on` is the key the marker starts on, so a
/// chooser opens where the session already is. `verb` is what typing does:
/// `/model` narrows a list, `/sessions` searches one.
pub(crate) fn choose(
    screen: &mut ui::Screen,
    rx: &Receiver<Event>,
    title: &str,
    verb: &str,
    query: &str,
    on: Option<&str>,
    find: impl Fn(&str) -> Vec<(String, String)>,
) -> Option<String> {
    let mut query = query.trim().to_string();
    loop {
        let found = find(&query);
        if found.is_empty() {
            // The header is prose for a title; the sentence needs it lower.
            screen.output(&format!("no {} match {query:?}\n", title.to_lowercase()));
            query.clear();
            continue;
        }
        let rows: Vec<String> = found.iter().map(|(_, row)| row.clone()).collect();
        let header = if query.is_empty() {
            format!("{title} (↑↓ · Enter; type to {verb}; Esc)")
        } else {
            format!("{title} matching {query:?} (↑↓ · Enter; type to refine; Esc)")
        };
        screen.set_menu(&header, rows, false);
        if let Some(at) = on.and_then(|key| found.iter().position(|(k, _)| k == key)) {
            screen.menu_select(at);
        }
        match readline(screen, rx, &mut |_| {}) {
            Some(Input::Menu(at)) => {
                screen.clear_menu(false);
                return found.get(at).map(|(key, _)| key.clone());
            }
            Some(Input::Cancel) | None => {
                screen.clear_menu(true);
                return None;
            }
            Some(Input::Line(line)) => query = line.trim().to_string(),
            Some(Input::Explore) => continue,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use ratatui::crossterm::event::KeyModifiers;

    use crate::turn::Queue;
    use crate::ui;

    #[test]
    fn a_chooser_query_narrows_by_spec_not_by_row() {
        let items = vec![
            (
                "deepseek/deepseek-chat".to_string(),
                "deepseek/deepseek-chat  (128000 ctx)".to_string(),
            ),
            ("local/qwen3".to_string(), "local/qwen3".to_string()),
            (
                "opencode-go/big".to_string(),
                "opencode-go/big  (200000 ctx)".to_string(),
            ),
        ];
        let keys = |query: &str| -> Vec<String> {
            narrow(&items, query)
                .into_iter()
                .map(|(key, _)| key)
                .collect()
        };
        assert_eq!(
            keys(""),
            vec!["deepseek/deepseek-chat", "local/qwen3", "opencode-go/big"]
        );
        assert_eq!(keys("DEEP"), vec!["deepseek/deepseek-chat"]);
        assert_eq!(keys("local"), vec!["local/qwen3"]);
        assert_eq!(keys("go/"), vec!["opencode-go/big"]);
        // The context window is drawn, not searched.
        assert!(keys("ctx").is_empty());
        assert!(keys("open/code").is_empty());
    }

    /// The keys a screen would receive for `text`, as characters.
    fn report_through(report: &mut CursorReport, text: &str) -> String {
        let mut out = String::new();
        for c in text.chars() {
            let modifiers = if c.is_uppercase() {
                KeyModifiers::SHIFT
            } else {
                KeyModifiers::NONE
            };
            for key in report.feed(KeyEvent::new(KeyCode::Char(c), modifiers)) {
                if let KeyCode::Char(c) = key.code {
                    out.push(c);
                }
            }
        }
        out
    }

    #[test]
    fn a_split_cursor_report_is_not_typing() {
        let mut report = CursorReport::default();
        // The escape was read as a key press; the rest is the report.
        assert_eq!(report_through(&mut report, "[18;1R"), "");
        // The next keys are typing again.
        assert_eq!(report_through(&mut report, "hi"), "hi");
    }

    #[test]
    fn typing_that_starts_like_a_report_is_kept() {
        let mut report = CursorReport::default();
        assert_eq!(report_through(&mut report, "[x"), "[x");
        assert_eq!(report_through(&mut report, "[1]"), "[1]");
        // A run that stops matching comes through whole, in order.
        assert_eq!(report_through(&mut report, "[18;1x"), "[18;1x");
        // Plain text never waits.
        assert_eq!(report_through(&mut report, "hello"), "hello");
    }

    #[test]
    fn up_takes_back_only_the_lines_the_turn_has_not_read() {
        let queue: Queue = Arc::new(Mutex::new(VecDeque::new()));
        let queued = [
            Pending {
                line: "read".into(),
                turn: true,
            },
            Pending {
                line: "/help".into(),
                turn: false,
            },
            Pending {
                line: "waiting".into(),
                turn: true,
            },
        ];
        queue.lock().unwrap().push_back("waiting".into());
        let (kept, read) = take_back(&queue, &queued);
        assert_eq!(kept, vec!["/help", "waiting"]);
        assert_eq!(read, 1, "one line was read and its echo is still owed");
        assert!(queue.lock().unwrap().is_empty(), "the rest is taken back");
    }

    #[test]
    fn a_recalled_queue_lands_above_what_was_typed() {
        let mut screen = ui::Screen::test_screen(40, 10);
        screen.insert_text("typed");
        recall_lines(&mut screen, &["one".to_string(), "two".to_string()]);
        // The waiting lines came first, so they would have been read first.
        assert_eq!(screen.take(), "one\ntwo\ntyped");
        // Nothing typed: the queue is the whole line, and nothing is inserted
        // for an empty one.
        recall_lines(&mut screen, &["only".to_string()]);
        assert_eq!(screen.take(), "only");
        recall_lines(&mut screen, &[]);
        assert!(screen.is_empty());
    }
}
