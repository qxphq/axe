//! The turn loop: what one user message sets off, until the model answers
//! without calling a tool. A round is one request; between rounds a typed line
//! joins the history and the tool calls the last reply named run.
//!
//! The sinks are the two ways a turn is shown — the screen's channel, and
//! stdout when there is no terminal — so the loop itself never draws.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use ratatui::text::Line;
use serde_json::{json, Value};

use crate::api;
use crate::http::{self, Cancel};
use crate::markdown;
use crate::render::{call_line, detail, failed, mark, plain_text, Paint};
use crate::tools;
use crate::ui;

/// Where a turn's output goes: straight to stdout when piped, onto the channel
/// when a prompt is on screen.
pub trait Sink {
    /// Streamed model text. `TextSink` owns the spacing: finished lines go out
    /// permanently, the unfinished one is exposed as a tail.
    fn text(&mut self, text: &str);
    /// Streamed reasoning. Shown or not is the sink's business; when it is
    /// hidden, one line with its duration stands in for it.
    fn reasoning(&mut self, text: &str);
    /// A transient label for the live region: "⋯ Thinking", a running command.
    fn status(&mut self, text: &str);
    /// One tool call, finished: the call and its outcome on one line, and the
    /// failure's own words beneath it.
    fn echo(&mut self, call: &[(Paint, String)], mark: (Paint, &'static str), error: Option<&str>);
    /// A line typed during the turn, which the turn has just appended to the
    /// history. Piped output has no queue, and nothing to say.
    fn user(&mut self, _line: &str) {}
    /// A round's text is over; close its last line if it wrote one.
    fn end_round(&mut self);
}

#[derive(Default)]
pub struct PlainSink {
    out: TextSink,
    md: markdown::Stream,
    /// The line being written: the stream ends wherever the provider felt
    /// like it, and a line is only a line once its newline arrives.
    partial: String,
    /// The last block written: the blank line between two blocks is the
    /// writer's, and `ui::separated` decides where it goes, so stdout and the
    /// screen space the transcript the same way.
    last: Option<ui::Block>,
    /// The next block starts at the top, dropping the blank row a reply carries
    /// to separate it from what came before: on a run's first answer there is
    /// nothing above it.
    flush_top: bool,
}

impl PlainSink {
    /// The sink for the first answer of a run, whose block sits flush at the
    /// top of stdout.
    pub fn flush_top() -> Self {
        PlainSink {
            flush_top: true,
            ..PlainSink::default()
        }
    }

    fn write(&mut self, lines: Vec<Line<'static>>, block: ui::Block) {
        if lines.is_empty() {
            return;
        }
        let mut buf = String::new();
        // The block the session opens with keeps the margin `TextSink` puts
        // there; every later one is separated by the same rule the screen uses.
        if self.last.is_some() && ui::separated(self.last, block) {
            buf.push('\n');
        }
        self.last = Some(block);
        for line in &lines {
            buf.push_str(&markdown::ansi(line));
            buf.push('\n');
        }
        let mut done = String::new();
        self.out.push(&buf, &mut done);
        if std::mem::take(&mut self.flush_top) {
            trim_top(&mut done);
        }
        print!("{done}");
        io::stdout().flush().ok();
    }

    fn flush(&mut self) {
        let mut lines = Vec::new();
        let rest = std::mem::take(&mut self.partial);
        if !rest.is_empty() {
            self.md.push(&rest, 0, &mut lines);
        }
        self.md.flush(0, &mut lines);
        self.write(lines, ui::Block::Text);
    }
}

/// The one blank row `TextSink` puts before a reply is the row between two
/// blocks. A run's first block has nothing above it, so that row is dropped.
fn trim_top(done: &mut String) {
    if done.starts_with('\n') {
        done.remove(0);
    }
}

impl Sink for PlainSink {
    fn text(&mut self, text: &str) {
        self.partial.push_str(text);
        let Some(last) = self.partial.rfind('\n') else {
            return;
        };
        let finished: String = self.partial.drain(..=last).collect();
        for line in ui::split(&finished) {
            let mut lines = Vec::new();
            self.md.push(line, 0, &mut lines);
            self.write(lines, ui::Block::Text);
        }
    }

    fn status(&mut self, _text: &str) {}

    fn echo(&mut self, call: &[(Paint, String)], mark: (Paint, &'static str), error: Option<&str>) {
        self.flush();
        // A tool call is the harness's account of the turn, not the answer: a
        // piped run keeps stdout for the model's text and puts this on stderr,
        // where it is visible when asked for and never inside the answer.
        if self.last.is_some() && ui::separated(self.last, ui::Block::Tool) {
            eprintln!();
        }
        self.last = Some(ui::Block::Tool);
        let mut line = String::new();
        for (paint, text) in call {
            line.push_str(&paint.ansi(text));
        }
        line.push(' ');
        line.push_str(&mark.0.ansi(mark.1));
        eprintln!("{line}");
        if let Some(error) = error {
            eprintln!("    {}", Paint::Red.ansi(error));
        }
        io::stderr().flush().ok();
    }

    fn end_round(&mut self) {
        self.flush();
        self.out.commit();
        if self.out.take_wrote() {
            println!();
        }
    }

    fn reasoning(&mut self, _text: &str) {}
}

/// Lines typed during a turn, waiting for the boundary that lets them join it.
/// One slot: while a line is pending the prompt refuses a second one.
pub type Queue = Arc<Mutex<VecDeque<String>>>;

/// What a turn's queue holds now, oldest first. Empty without a queue, and
/// empty rather than a failed turn when the lock is poisoned.
fn take_queue(queue: Option<&Queue>) -> Vec<String> {
    let Some(queue) = queue else {
        return Vec::new();
    };
    match queue.lock() {
        Ok(mut lines) => lines.drain(..).collect(),
        Err(_) => Vec::new(),
    }
}

/// One turn, from the request that starts it to the answer that ends it. The
/// provider is transport here — endpoint, key, headers — because that is all a
/// turn talks to; the session's model, its extra body fields and its mode are
/// the rest.
pub struct Turn<'a> {
    pub ep: &'a http::Endpoint,
    pub key: Option<&'a str>,
    pub headers: &'a [(String, String)],
    pub model: &'a str,
    /// Request-body fields for the thinking level, when one is set.
    pub extra: Option<&'a Value>,
    pub mode: tools::Mode,
    /// A line typed while the turn runs, taken at the next round boundary.
    pub queue: Option<&'a Queue>,
    pub cancel: &'a Cancel,
}

/// The history a turn appends to. The live session writes each message to its
/// log as it goes; the turn only appends, and never decides what is persisted.
pub trait History {
    fn push(&mut self, message: Value);
    fn messages(&self) -> &[Value];
}

/// Rounds until the model answers without calling a tool. Ends early on an
/// interrupt, leaving the user message unanswered, which is what happened.
pub fn run_turn(
    turn: &Turn,
    history: &mut dyn History,
    sink: &mut dyn Sink,
) -> Result<Option<u64>, String> {
    let tools = tools::definitions();
    // The last round's own count. It measures the whole request, so it is the
    // fill; `None` when no round reported one.
    let mut used = None;

    loop {
        // An interrupt ends the turn here too: no further round is started.
        if turn.cancel.cancelled() {
            return Ok(used);
        }
        // A line typed during the turn joins it here, at the only boundary
        // there is: every tool result the last reply named has been answered,
        // and this request does not exist yet, so the model reads it before
        // answering instead of after the turn. A round already streaming cannot
        // see it, and a reply that ends the turn has no round to reach.
        for line in take_queue(turn.queue) {
            history.push(json!({ "role": "user", "content": line }));
            sink.user(&line);
        }
        // The request is in flight and nothing may reach the sink for the whole
        // round: a provider prefills a long history before it streams a byte,
        // and the arguments of a tool call arrive as deltas the sinks never
        // see. The row says what the turn is doing until a more specific state
        // — the answer, a hidden thought, the command being run — replaces it.
        sink.status("⋯ Thinking");

        // Both callbacks borrow the sink, so it is shared rather than captured
        // twice. The borrow ends with the block, so the turn can still report
        // through it after the stream is over.
        let result = {
            let sink = RefCell::new(&mut *sink);
            api::stream_chat(
                turn.ep,
                turn.key,
                turn.headers,
                turn.model,
                history.messages(),
                tools,
                turn.extra,
                turn.cancel,
                |text| sink.borrow_mut().text(text),
                |text| sink.borrow_mut().reasoning(text),
            )
        };
        let reply = match result {
            Ok(reply) => reply,
            // Closing the socket under a read is how the interrupt unblocks
            // it; the error is the interruption, not a failure. What arrived
            // is still printed, but the turn is over.
            Err(_) if turn.cancel.cancelled() => {
                sink.end_round();
                return Ok(used);
            }
            Err(e) => return Err(e),
        };
        if let Some(count) = &reply.usage {
            used = Some(count.prompt + count.completion);
        }
        sink.end_round();
        // A shut-down read can also end the stream without an error. A cut
        // reply is not an assistant message, and its calls never run.
        if turn.cancel.cancelled() {
            return Ok(used);
        }

        if reply.calls.is_empty() {
            history.push(assistant_message(&reply));
            return Ok(used);
        }

        let calls: Vec<Value> = reply
            .calls
            .iter()
            .map(|call| {
                json!({
                    "id": call.id,
                    "type": "function",
                    "function": {
                        "name": call.name,
                        "arguments": call.arguments,
                    }
                })
            })
            .collect();
        let mut message = assistant_message(&reply);
        message["tool_calls"] = json!(calls);
        history.push(message);

        // An interrupt can land before a call runs or between two of them.
        // Every id the assistant message named still owes a result: a strict
        // provider refuses the next request until each `tool_call_id` is
        // answered, and the history is what a resumed session replays.
        let mut answered = 0;
        for call in &reply.calls {
            if turn.cancel.cancelled() {
                break;
            }
            let echo = call_line(call, turn.mode.explore());
            // What is running, while it runs: the finished line appears below
            // once there is a result to put on it.
            sink.status(&plain_text(&echo));
            let result = tools::execute(&call.name, &call.arguments, turn.cancel, turn.mode);
            let error = failed(&call.name, &result).then(|| detail(&result));
            sink.echo(&echo, mark(&call.name, &result), error.as_deref());
            history.push(tool_message(&call.id, &result));
            answered += 1;
        }
        for message in unanswered(&reply.calls, answered) {
            history.push(message);
        }
        if answered < reply.calls.len() {
            return Ok(used);
        }
    }
}

/// A tool result as it goes back into the history.
fn tool_message(id: &str, content: &str) -> Value {
    json!({ "role": "tool", "tool_call_id": id, "content": content })
}

/// The calls of an interrupted round that never ran, as results. The provider
/// wants one per `tool_call_id` before the next request, and a resumed session
/// replays whatever the log holds, so a gap here outlives the turn.
fn unanswered(calls: &[api::ToolCall], answered: usize) -> Vec<Value> {
    calls[answered.min(calls.len())..]
        .iter()
        .map(|call| {
            tool_message(
                &call.id,
                "error: interrupted by user (ESC); the call was not run",
            )
        })
        .collect()
}

/// The assistant message as it goes back into the history. Thinking-mode
/// providers (DeepSeek with `tools`, and the servers that share the extension)
/// reject the next request unless the reasoning they streamed is returned.
fn assistant_message(reply: &api::Reply) -> Value {
    let mut message = json!({ "role": "assistant", "content": reply.text });
    if !reply.reasoning.is_empty() {
        message["reasoning_content"] = json!(reply.reasoning);
    }
    message
}

/// Streams model text to stdout, holding back newlines at the edges. Providers
/// pad `content` differently (leading or trailing blank lines); the harness
/// owns spacing, so those would otherwise widen the gap between blocks by an
/// amount that varies per reply.
#[derive(Default)]
pub struct TextSink {
    started: bool,
    /// Blank lines to emit before the next text: a provider pads `content`
    /// differently, and the gap is only real once the next line starts.
    pending: usize,
    wrote: bool,
}

impl TextSink {
    /// Feed streamed text and append whatever may be printed now.
    ///
    /// Text is emitted as it arrives: holding a line until its newline would
    /// mean nothing appears until the line ends.
    pub fn push(&mut self, text: &str, out: &mut String) {
        for ch in text.chars() {
            if ch == '\n' {
                if self.started {
                    self.pending += 1;
                }
                continue;
            }
            if !self.started {
                self.started = true;
                // Piped output has no screen to put the blank row between two
                // blocks, so the reply carries its own. Emitted here, not per
                // round: a round with no text must not widen the gap between
                // two tool calls. `Screen::write` inserts its own separator and
                // reads this one as the empty line it is.
                out.push('\n');
            }
            for _ in 0..self.pending {
                out.push('\n');
            }
            self.pending = 0;
            self.wrote = true;
            out.push(ch);
        }
    }

    /// Whether the reply wrote anything since the last call, clearing it.
    pub fn take_wrote(&mut self) -> bool {
        std::mem::take(&mut self.wrote)
    }

    /// End the reply: blank lines held back at its tail are dropped, since the
    /// harness owns the spacing between blocks.
    pub fn commit(&mut self) {
        self.pending = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_interrupted_round_answers_every_call_it_named() {
        let calls = vec![
            api::ToolCall {
                id: "a".into(),
                name: "read".into(),
                arguments: "{}".into(),
            },
            api::ToolCall {
                id: "b".into(),
                name: "bash".into(),
                arguments: "{}".into(),
            },
        ];
        // A round that ran both owes nothing.
        assert!(unanswered(&calls, 2).is_empty());
        // One cut short answers the rest, so the next request is accepted.
        let rest = unanswered(&calls, 1);
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0]["role"], "tool");
        assert_eq!(rest[0]["tool_call_id"], "b");
        assert!(rest[0]["content"].as_str().unwrap().contains("interrupted"));
        // A round interrupted before the first call still answers both.
        assert_eq!(unanswered(&calls, 0).len(), 2);
    }

    #[test]
    fn text_sink_normalizes_reply_spacing() {
        let mut sink = TextSink::default();
        let mut out = String::new();
        sink.push("\n\nhello", &mut out);
        sink.push(" world\n\n", &mut out);
        sink.commit();
        assert_eq!(out, "\nhello world");
        assert!(sink.wrote);
    }

    /// What the turn showed, in order: enough to assert on a whole turn
    /// without a terminal.
    #[derive(Default)]
    struct Recorder {
        seen: Vec<String>,
    }

    impl Sink for Recorder {
        fn text(&mut self, text: &str) {
            self.seen.push(format!("text:{text}"));
        }
        fn reasoning(&mut self, text: &str) {
            self.seen.push(format!("think:{text}"));
        }
        fn status(&mut self, text: &str) {
            self.seen.push(format!("status:{text}"));
        }
        fn echo(
            &mut self,
            call: &[(Paint, String)],
            mark: (Paint, &'static str),
            error: Option<&str>,
        ) {
            let line: String = call.iter().map(|(_, text)| text.as_str()).collect();
            self.seen
                .push(format!("echo:{line} {} {:?}", mark.1, error));
        }
        fn user(&mut self, line: &str) {
            self.seen.push(format!("user:{line}"));
        }
        fn end_round(&mut self) {
            self.seen.push("round".into());
        }
    }

    struct Log(Vec<Value>);

    impl History for Log {
        fn push(&mut self, message: Value) {
            self.0.push(message);
        }
        fn messages(&self) -> &[Value] {
            &self.0
        }
    }

    /// A sink and a history, for the tests that run a whole turn.
    fn log() -> Log {
        Log(vec![json!({ "role": "user", "content": "hi" })])
    }

    #[test]
    fn a_turn_runs_the_calls_a_round_named_and_asks_again() {
        use crate::http::support::{sse, Server};

        let server = Server::new(vec![
            sse(&[
                r#"{"choices":[{"delta":{"content":"let me look"}}]}"#,
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"about","arguments":"{}"}}]}}]}"#,
                r#"{"usage":{"prompt_tokens":10,"completion_tokens":2}}"#,
                "[DONE]",
            ]),
            sse(&[r#"{"choices":[{"delta":{"content":"done"}}]}"#, "[DONE]"]),
        ]);
        let cancel = Cancel::new();
        let turn = Turn {
            ep: &server.ep,
            key: None,
            headers: &[],
            model: "m",
            extra: None,
            mode: tools::Mode::Normal,
            queue: None,
            cancel: &cancel,
        };
        let mut history = log();
        let mut sink = Recorder::default();
        let used = run_turn(&turn, &mut history, &mut sink).unwrap();

        assert_eq!(used, Some(12));
        // The call is answered before the second request, and the answer that
        // ends the turn is the assistant message.
        assert_eq!(history.0[1]["tool_calls"][0]["function"]["name"], "about");
        assert_eq!(history.0[2]["role"], "tool");
        assert_eq!(history.0[2]["tool_call_id"], "c1");
        assert_eq!(history.0[3]["content"], "done");
        let echoed = sink
            .seen
            .iter()
            .find(|e| e.starts_with("echo:"))
            .expect("the call is shown");
        assert!(echoed.contains("about") && echoed.contains('✓'), "{echoed}");
        assert!(
            sink.seen.iter().any(|e| e == "status:⋯ Thinking"),
            "{:?}",
            sink.seen
        );
        assert_eq!(sink.seen.iter().filter(|e| *e == "round").count(), 2);
    }

    #[test]
    fn a_queued_line_joins_the_turn_at_the_next_round() {
        use crate::http::support::{sse, Server};

        let server = Server::new(vec![sse(&[
            r#"{"choices":[{"delta":{"content":"ok"}}]}"#,
            "[DONE]",
        ])]);
        let queue: Queue = Arc::new(Mutex::new(VecDeque::new()));
        queue.lock().unwrap().push_back("and this".into());
        let cancel = Cancel::new();
        let turn = Turn {
            ep: &server.ep,
            key: None,
            headers: &[],
            model: "m",
            extra: None,
            mode: tools::Mode::Normal,
            queue: Some(&queue),
            cancel: &cancel,
        };
        let mut history = log();
        let mut sink = Recorder::default();
        run_turn(&turn, &mut history, &mut sink).unwrap();

        assert_eq!(history.0[1]["role"], "user");
        assert_eq!(history.0[1]["content"], "and this");
        assert_eq!(history.0[2]["content"], "ok");
        assert!(sink.seen.contains(&"user:and this".to_string()));
        assert!(queue.lock().unwrap().is_empty());
    }

    #[test]
    fn an_interrupt_before_a_round_starts_asks_nothing() {
        let server = crate::http::support::Server::new(Vec::new());
        let cancel = Cancel::new();
        cancel.cancel();
        let turn = Turn {
            ep: &server.ep,
            key: None,
            headers: &[],
            model: "m",
            extra: None,
            mode: tools::Mode::Normal,
            queue: None,
            cancel: &cancel,
        };
        let mut history = log();
        let mut sink = Recorder::default();
        assert_eq!(run_turn(&turn, &mut history, &mut sink).unwrap(), None);
        assert_eq!(history.0.len(), 1);
        assert!(sink.seen.is_empty());
    }

    #[test]
    fn a_queue_is_drained_once_and_missing_queues_are_empty() {
        let queue: Queue = Arc::new(Mutex::new(VecDeque::new()));
        queue.lock().unwrap().push_back("a".into());
        queue.lock().unwrap().push_back("b".into());
        assert_eq!(take_queue(Some(&queue)), vec!["a", "b"]);
        assert!(take_queue(Some(&queue)).is_empty());
        assert!(take_queue(None).is_empty());
    }

    #[test]
    fn an_assistant_message_returns_the_reasoning_it_streamed() {
        let reply = api::Reply {
            text: "answer".into(),
            reasoning: "thought".into(),
            calls: Vec::new(),
            usage: None,
            finish_reason: None,
        };
        let message = assistant_message(&reply);
        assert_eq!(message["content"], "answer");
        assert_eq!(message["reasoning_content"], "thought");

        let plain = api::Reply {
            reasoning: String::new(),
            ..reply
        };
        assert!(assistant_message(&plain).get("reasoning_content").is_none());
    }

    #[test]
    fn a_text_sink_takes_its_blank_rows_once() {
        let mut sink = TextSink::default();
        let mut out = String::new();
        // No newline: nothing is written yet, and nothing is claimed.
        sink.push("", &mut out);
        assert!(!sink.take_wrote());
        sink.push("first", &mut out);
        assert!(sink.take_wrote());
        assert!(!sink.take_wrote());
        sink.push("\n\n\nsecond", &mut out);
        sink.commit();
        assert_eq!(out, "\nfirst\n\n\nsecond");
    }

    #[test]
    fn a_plain_sink_spaces_blocks_the_way_the_screen_does() {
        let mut sink = PlainSink::default();
        // Text that ends a block, then a tool line, then text again: the blank
        // rows between them come from `ui::separated`, not from the provider.
        sink.text("hello\n\n\n");
        sink.end_round();
        assert!(matches!(sink.last, Some(ui::Block::Text)));
        sink.echo(
            &[(Paint::Cyan, "read x".to_string())],
            (Paint::Green, "ok"),
            Some("boom"),
        );
        assert!(matches!(sink.last, Some(ui::Block::Tool)));
        sink.text("after");
        sink.reasoning("hidden");
        sink.status("ignored");
        sink.end_round();
        assert!(matches!(sink.last, Some(ui::Block::Text)));
    }

    #[test]
    fn a_runs_first_block_drops_one_leading_blank_row() {
        let mut done = "\nhi".to_string();
        trim_top(&mut done);
        assert_eq!(done, "hi");
        // One row is the writer's; a second is the model's own and stays.
        let mut done = "\n\nhi".to_string();
        trim_top(&mut done);
        assert_eq!(done, "\nhi");
        // Nothing above and nothing to drop.
        let mut done = "hi".to_string();
        trim_top(&mut done);
        assert_eq!(done, "hi");
    }
}
